//! Connect client — registration, heartbeat, metrics push, relay setup.
//!
//! All HTTP calls use mTLS (client certificate loaded from `AgentConfig`).
//! Reconnect uses truncated exponential backoff (max 5 min).

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use backoff::{backoff::Backoff, ExponentialBackoffBuilder};
use reqwest::{Certificate, Client, Identity};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::{
    metrics,
    relay::{self, RelayParams},
};

// ── Constants ────────────────────────────────────────────────────────────────

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const METRICS_INTERVAL: Duration = Duration::from_secs(60);

// ── Config ───────────────────────────────────────────────────────────────────

/// Configuration for the Connect client.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Connect API base URL (default: `https://connect.ozma.dev/api/v1`).
    pub api_base: String,
    /// Bearer token for the Connect API.
    pub token: String,
    /// Unique machine identifier (UUID or hostname-derived).
    pub machine_id: String,
    /// Agent capabilities, e.g. `["hid", "stream", "audio"]`.
    pub capabilities: Vec<String>,
    /// Agent version string.
    pub version: String,
    /// Path to the PEM-encoded client certificate (mTLS).
    pub client_cert_pem: PathBuf,
    /// Path to the PEM-encoded client private key (mTLS).
    pub client_key_pem: PathBuf,
    /// Optional path to a custom CA certificate to trust.
    pub ca_cert_pem: Option<PathBuf>,
    /// WireGuard private key (base64) for the relay tunnel.
    /// If empty, relay setup is skipped.
    pub wg_private_key: String,
    /// WireGuard public key (base64) advertised to the relay.
    pub wg_public_key: String,
}

// ── Wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct RegisterRequest<'a> {
    machine_id:   &'a str,
    pubkey:       &'a str,
    capabilities: &'a [String],
    version:      &'a str,
    hostname:     String,
    platform:     &'static str,
    arch:         &'static str,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    ok:    bool,
    relay: Option<RelayParams>,
}

#[derive(Debug, Serialize)]
struct HeartbeatRequest<'a> {
    machine_id: &'a str,
    status:     &'static str,
    relay_ip:   &'a str,
}

#[derive(Debug, Serialize)]
struct MetricsPushRequest<'a> {
    machine_id: &'a str,
    metrics:    &'a metrics::SystemMetrics,
}

// ── Client ───────────────────────────────────────────────────────────────────

/// Connect client handle.
///
/// Call [`ConnectClient::start`] to spawn background tasks.
/// Drop the handle (or call [`ConnectClient::stop`]) to shut down.
pub struct ConnectClient {
    cfg:      Arc<AgentConfig>,
    http:     Client,
    relay_ip: Arc<std::sync::Mutex<String>>,
    running:  Arc<AtomicBool>,
}

impl ConnectClient {
    /// Build a new client from `cfg`.
    ///
    /// Loads the mTLS identity and optional CA cert from disk.
    /// If the cert files do not exist (dev/test environment), falls back to
    /// a plain TLS client so the binary still starts without certs on disk.
    pub fn new(cfg: AgentConfig) -> Result<Self> {
        let http = build_http_client(&cfg)?;
        Ok(Self {
            cfg:      Arc::new(cfg),
            http,
            relay_ip: Arc::new(std::sync::Mutex::new(String::new())),
            running:  Arc::new(AtomicBool::new(false)),
        })
    }

    /// Register with Connect, set up the relay tunnel, then start the
    /// heartbeat and metrics background tasks.
    ///
    /// Returns immediately after spawning tasks.
    pub async fn start(&self) -> Result<()> {
        if self.cfg.token.is_empty() {
            info!("No Connect token configured — skipping registration");
            return Ok(());
        }

        self.running.store(true, Ordering::SeqCst);

        // Initial registration with backoff
        let relay_params = self.register_with_backoff().await?;

        // Set up WireGuard relay tunnel
        if let Some(params) = relay_params {
            if !self.cfg.wg_private_key.is_empty() {
                match relay::setup(&params, &self.cfg.wg_private_key).await {
                    Ok(ip) => *self.relay_ip.lock().unwrap() = ip,
                    Err(e) => warn!("Relay setup failed (non-fatal): {e}"),
                }
            }
        }

        // Spawn heartbeat task
        {
            let cfg      = Arc::clone(&self.cfg);
            let http     = self.http.clone();
            let relay_ip = Arc::clone(&self.relay_ip);
            let running  = Arc::clone(&self.running);
            tokio::spawn(async move {
                heartbeat_loop(cfg, http, relay_ip, running).await;
            });
        }

        // Spawn metrics task
        {
            let cfg     = Arc::clone(&self.cfg);
            let http    = self.http.clone();
            let running = Arc::clone(&self.running);
            tokio::spawn(async move {
                metrics_loop(cfg, http, running).await;
            });
        }

        Ok(())
    }

    /// Gracefully stop background tasks and tear down the relay tunnel.
    pub async fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        relay::teardown().await;

        // Best-effort offline heartbeat
        let relay_ip = self.relay_ip.lock().unwrap().clone();
        let _ = self
            .http
            .post(format!("{}/agents/heartbeat", self.cfg.api_base))
            .bearer_auth(&self.cfg.token)
            .json(&HeartbeatRequest {
                machine_id: &self.cfg.machine_id,
                status:     "offline",
                relay_ip:   &relay_ip,
            })
            .send()
            .await;
    }

    // ── Private helpers ───────────────────────────────────────────────────

    async fn register_with_backoff(&self) -> Result<Option<RelayParams>> {
        let mut bo = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_secs(2))
            .with_multiplier(2.0)
            .with_max_interval(Duration::from_secs(300))
            .with_max_elapsed_time(None) // retry forever
            .build();

        loop {
            match self.register_once().await {
                Ok(relay) => return Ok(relay),
                Err(e) => {
                    let wait = bo.next_backoff().unwrap_or(Duration::from_secs(300));
                    warn!("Registration failed ({e}), retrying in {wait:.1?}");
                    sleep(wait).await;
                }
            }
        }
    }

    async fn register_once(&self) -> Result<Option<RelayParams>> {
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".into());

        let body = RegisterRequest {
            machine_id:   &self.cfg.machine_id,
            pubkey:       &self.cfg.wg_public_key,
            capabilities: &self.cfg.capabilities,
            version:      &self.cfg.version,
            hostname,
            platform:     std::env::consts::OS,
            arch:         std::env::consts::ARCH,
        };

        let resp = self
            .http
            .post(format!("{}/agents/register", self.cfg.api_base))
            .bearer_auth(&self.cfg.token)
            .json(&body)
            .send()
            .await
            .context("HTTP POST /agents/register")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Register returned {status}: {text}");
        }

        let reg: RegisterResponse =
            resp.json().await.context("Deserialise register response")?;
        if !reg.ok {
            anyhow::bail!("Register response ok=false");
        }

        info!(machine_id = %self.cfg.machine_id, "Registered with Connect");
        Ok(reg.relay)
    }
}

// ── Background loops ─────────────────────────────────────────────────────────

async fn heartbeat_loop(
    cfg:      Arc<AgentConfig>,
    http:     Client,
    relay_ip: Arc<std::sync::Mutex<String>>,
    running:  Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        sleep(HEARTBEAT_INTERVAL).await;
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let ip = relay_ip.lock().unwrap().clone();
        let result = http
            .post(format!("{}/agents/heartbeat", cfg.api_base))
            .bearer_auth(&cfg.token)
            .json(&HeartbeatRequest {
                machine_id: &cfg.machine_id,
                status:     "online",
                relay_ip:   &ip,
            })
            .send()
            .await;

        match result {
            Ok(r) if r.status().is_success() => {
                debug!(machine_id = %cfg.machine_id, "Heartbeat OK");
            }
            Ok(r) => warn!("Heartbeat non-2xx: {}", r.status()),
            Err(e) => warn!("Heartbeat error: {e}"),
        }
    }
}

async fn metrics_loop(
    cfg:     Arc<AgentConfig>,
    http:    Client,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        sleep(METRICS_INTERVAL).await;
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let snapshot = metrics::collect();
        let result = http
            .post(format!("{}/agents/metrics", cfg.api_base))
            .bearer_auth(&cfg.token)
            .json(&MetricsPushRequest {
                machine_id: &cfg.machine_id,
                metrics:    &snapshot,
            })
            .send()
            .await;

        match result {
            Ok(r) if r.status().is_success() => {
                debug!(machine_id = %cfg.machine_id, "Metrics pushed");
            }
            Ok(r) => warn!("Metrics push non-2xx: {}", r.status()),
            Err(e) => warn!("Metrics push error: {e}"),
        }
    }
}

// ── HTTP client builder ───────────────────────────────────────────────────────

/// Build a `reqwest::Client` with mTLS client certificate and optional custom CA.
///
/// Falls back to a plain TLS client if the cert files are absent so the
/// binary starts cleanly in dev/CI environments without provisioned certs.
fn build_http_client(cfg: &AgentConfig) -> Result<Client> {
    use std::fs;

    let mut builder = Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10));

    // mTLS identity — skip gracefully if files are absent
    if cfg.client_cert_pem.exists() && cfg.client_key_pem.exists() {
        let cert_pem = fs::read(&cfg.client_cert_pem)
            .with_context(|| format!("Read client cert {:?}", cfg.client_cert_pem))?;
        let key_pem = fs::read(&cfg.client_key_pem)
            .with_context(|| format!("Read client key {:?}", cfg.client_key_pem))?;
        let mut pem = cert_pem;
        pem.extend_from_slice(&key_pem);
        let identity = Identity::from_pem(&pem).context("Build mTLS identity from PEM")?;
        builder = builder.identity(identity);
    } else {
        warn!(
            cert = ?cfg.client_cert_pem,
            "mTLS cert not found — connecting without client certificate"
        );
    }

    // Optional custom CA
    if let Some(ca_path) = &cfg.ca_cert_pem {
        let ca_pem = fs::read(ca_path)
            .with_context(|| format!("Read CA cert {ca_path:?}"))?;
        let ca_cert = Certificate::from_pem(&ca_pem).context("Parse CA cert")?;
        builder = builder.add_root_certificate(ca_cert);
    }

    builder.build().context("Build reqwest client")
}
