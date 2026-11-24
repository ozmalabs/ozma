// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Report collected software inventory to the controller.
//!
//! The controller stores one latest snapshot per host at
//! `POST /api/v1/software/hosts/{host_id}/inventory`. Reporting is opt-in from
//! the node's perspective: it runs only when a controller URL is configured.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::collect_inventory;
use super::model::SoftwareRecord;

/// Default interval between software inventory reports.
pub const DEFAULT_REPORT_INTERVAL: Duration = Duration::from_secs(3600);
/// Retry delay after a failed report (controller restart, auth, transient I/O).
pub const RETRY_INTERVAL: Duration = Duration::from_secs(300);

/// Errors raised while collecting or reporting inventory.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("controller URL must not be empty")]
    EmptyBaseUrl,
    #[error("host id must not be empty")]
    EmptyHostId,
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("controller rejected inventory: HTTP {status}: {body}")]
    Rejected { status: u16, body: String },
    #[error("software inventory collection task failed: {0}")]
    Collection(String),
}

/// Seconds since the Unix epoch as a float, matching the controller payload.
pub fn unix_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// Build the JSON body accepted by the controller inventory endpoint.
pub fn inventory_payload(records: &[SoftwareRecord], reported_at: f64) -> Value {
    json!({
        "records": records,
        "reported_at": reported_at,
    })
}

/// Percent-encode one URL path segment.
fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

/// Client for the controller's fleet software inventory API.
#[derive(Clone, Debug)]
pub struct InventoryReporter {
    client: Client,
    base_url: String,
    host_id: String,
}

impl InventoryReporter {
    /// Create a reporter for one controller and host.
    pub fn new(
        base_url: impl Into<String>,
        host_id: impl Into<String>,
    ) -> Result<Self, ReportError> {
        let base_url = base_url.into();
        if base_url.trim().is_empty() {
            return Err(ReportError::EmptyBaseUrl);
        }
        let host_id = host_id.into();
        if host_id.trim().is_empty() {
            return Err(ReportError::EmptyHostId);
        }
        let client = Client::builder().timeout(Duration::from_secs(15)).build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            host_id,
        })
    }

    /// Controller inventory endpoint for this host.
    pub fn url(&self) -> String {
        format!(
            "{}/api/v1/software/hosts/{}/inventory",
            self.base_url,
            encode_path_segment(&self.host_id)
        )
    }

    /// Host identifier reported by this client.
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// POST records to the controller.
    pub async fn report(&self, records: &[SoftwareRecord]) -> Result<u16, ReportError> {
        let body = inventory_payload(records, unix_timestamp());
        let response = self.client.post(self.url()).json(&body).send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(status.as_u16())
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(ReportError::Rejected {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Collect the full host inventory on a blocking task, then report it.
    ///
    /// Returns the number of records reported.
    pub async fn collect_and_report(&self) -> Result<usize, ReportError> {
        let records = tokio::task::spawn_blocking(collect_inventory)
            .await
            .map_err(|error| ReportError::Collection(error.to_string()))?;
        self.report(&records).await?;
        Ok(records.len())
    }

    /// Report immediately, then every `interval` until cancellation.
    ///
    /// A failed report retries sooner than the full interval so a controller
    /// that is restarting when the node boots does not leave the host without
    /// inventory for an hour.
    pub async fn run(&self, interval: Duration, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                break;
            }

            let delay = match self.collect_and_report().await {
                Ok(count) => {
                    info!(
                        host_id = %self.host_id,
                        records = count,
                        "Reported software inventory to controller"
                    );
                    interval
                }
                Err(error) => {
                    warn!(
                        host_id = %self.host_id,
                        error = %error,
                        "Software inventory report failed"
                    );
                    RETRY_INTERVAL.min(interval)
                }
            };

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
}

/// Convenience helper for one-shot reports.
pub async fn report_inventory(
    base_url: &str,
    host_id: &str,
    records: &[SoftwareRecord],
) -> Result<u16, ReportError> {
    InventoryReporter::new(base_url, host_id)?
        .report(records)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::software::model::{SoftwareScope, UpdateState};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn record() -> SoftwareRecord {
        SoftwareRecord {
            name: "Example Game".into(),
            source: "steam".into(),
            source_id: "70".into(),
            version: "100".into(),
            scope: SoftwareScope::User,
            managed_by: "steam".into(),
            update_state: UpdateState::ExternalManaged,
            ..SoftwareRecord::default()
        }
    }

    #[test]
    fn payload_matches_controller_shape() {
        let value = inventory_payload(&[record()], 42.5);
        assert_eq!(value["reported_at"], 42.5);
        assert_eq!(value["records"][0]["source"], "steam");
        assert_eq!(value["records"][0]["scope"], "user");
        assert_eq!(value["records"][0]["update_state"], "external_managed");
    }

    #[test]
    fn url_quotes_host_id() {
        let reporter = InventoryReporter::new("http://10.0.0.1:7380/", "host/a b").unwrap();
        assert_eq!(
            reporter.url(),
            "http://10.0.0.1:7380/api/v1/software/hosts/host%2Fa%20b/inventory"
        );
    }

    #[test]
    fn empty_inputs_are_rejected() {
        assert!(matches!(
            InventoryReporter::new("", "host"),
            Err(ReportError::EmptyBaseUrl)
        ));
        assert!(matches!(
            InventoryReporter::new("http://controller", ""),
            Err(ReportError::EmptyHostId)
        ));
    }

    #[tokio::test]
    async fn report_posts_the_inventory_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (body_tx, body_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(header_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&buffer[..header_end]);
                    let content_length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    if buffer.len() >= body_start + content_length {
                        let body = String::from_utf8_lossy(
                            &buffer[body_start..body_start + content_length],
                        )
                        .into_owned();
                        let _ = body_tx.send(body);
                        break;
                    }
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
        });

        let reporter = InventoryReporter::new(format!("http://{address}"), "node-1").unwrap();
        let status = reporter.report(&[record()]).await.unwrap();
        assert_eq!(status, 200);

        let body: Value = serde_json::from_str(&body_rx.await.unwrap()).unwrap();
        assert_eq!(body["records"][0]["source"], "steam");
        assert!(body["reported_at"].as_f64().unwrap() > 0.0);
    }
}
