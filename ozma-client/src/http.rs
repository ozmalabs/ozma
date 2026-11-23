// SPDX-License-Identifier: AGPL-3.0-only
//! Small HTTP helpers shared by the controller and node API clients.
//!
//! Both surfaces are plain JSON over HTTP(S). TLS is rustls-only: `reqwest` is
//! built without native-tls, and `insecure` is an explicit opt-in for local
//! bring-up with a self-signed controller or node API.

use std::time::Duration;

use ozma_protocol::ApiError;
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION};
use reqwest::{Client, ClientBuilder, RequestBuilder, Url};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::error;

pub(crate) const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Normalise a controller or node base URL.
///
/// Accepts `host:port` shorthand, strips a trailing slash and any query or
/// fragment, and requires an `http` or `https` scheme.
pub(crate) fn normalize_base_url(input: &str) -> Result<String, ApiError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(
            error::config("A controller or node URL is required").with_remediation(
                "Pass --controller (or set OZMA_CONTROLLER_URL), or pass --node for the node API.",
            ),
        );
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("http://{input}")
    };
    let mut url = Url::parse(&with_scheme).map_err(|error| {
        error::config(format!("Invalid base URL '{input}': {error}"))
            .with_remediation("Use a URL such as http://controller:7380.")
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(error::config(format!(
            "Unsupported URL scheme '{}'; use http or https",
            url.scheme()
        )));
    }
    if url.host_str().is_none() {
        return Err(error::config(format!("Base URL '{input}' has no host")));
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Build the request URL for an absolute API path against a normalised base.
pub(crate) fn join_url(base: &str, path: &str) -> Result<Url, ApiError> {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Url::parse(&format!("{base}{path}")).map_err(|error| {
        error::config(format!(
            "Could not build the API URL '{base}{path}': {error}"
        ))
    })
}

/// Build a rustls-only HTTP client with optional bearer auth.
pub(crate) fn build_client(
    token: Option<&str>,
    insecure: bool,
    timeout: Duration,
    no_proxy: bool,
) -> Result<Client, ApiError> {
    let mut builder = ClientBuilder::new()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .user_agent(concat!("ozma-client/", env!("CARGO_PKG_VERSION")));
    if insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if no_proxy {
        builder = builder.no_proxy();
    }

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        let token = token.trim();
        if !token.is_empty() {
            let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| {
                error::config(format!(
                    "The bearer token is not a valid HTTP header: {error}"
                ))
            })?;
            headers.insert(AUTHORIZATION, value);
        }
    }
    builder = builder.default_headers(headers);
    builder
        .build()
        .map_err(|error| error::config(format!("Could not create the HTTP client: {error}")))
}

/// Send a request and decode a successful JSON response.
pub(crate) async fn send_json<T: DeserializeOwned>(
    request: RequestBuilder,
    surface: &str,
) -> Result<T, ApiError> {
    let response = request.send().await.map_err(error::transport_error)?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|error| {
        error::controller(format!("Could not read the {surface} response: {error}"))
    })?;
    if !status.is_success() {
        return Err(error::http_response_error(status, &bytes));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        error::controller(format!("The {surface} returned invalid JSON: {error}")).with_details(
            json!({
                "status": status.as_u16(),
                "body": String::from_utf8_lossy(&bytes[..bytes.len().min(512)]),
            }),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_are_normalised() {
        assert_eq!(
            normalize_base_url("controller:7380/").unwrap(),
            "http://controller:7380"
        );
        assert_eq!(
            normalize_base_url(" https://controller.example/api/ ").unwrap(),
            "https://controller.example/api"
        );
        assert_eq!(
            normalize_base_url("http://[::1]:7380").unwrap(),
            "http://[::1]:7380"
        );
    }

    #[test]
    fn bad_base_urls_are_rejected() {
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("ftp://controller").is_err());
        assert!(normalize_base_url("http://").is_err());
    }

    #[test]
    fn paths_join_without_double_slashes() {
        let url = join_url("http://controller:7380", "/api/v1/nodes").unwrap();
        assert_eq!(url.as_str(), "http://controller:7380/api/v1/nodes");
    }
}
