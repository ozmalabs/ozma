// SPDX-License-Identifier: AGPL-3.0-only
//! Scoped development media origin for Cast HLS output.
//!
//! Put this behind an HTTPS reverse proxy before giving its URL to a Cast
//! receiver. The grant token is the bearer credential for media and revoke.

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get},
    Router,
};
use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;

const MAX_MEDIA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GRANTS: usize = 16;
const TOKEN_BYTES: usize = 32;

#[derive(Clone)]
pub struct GrantStore {
    grants: Arc<Mutex<HashMap<String, Grant>>>,
    lifetime: Duration,
}
struct Grant {
    root: PathBuf,
    expires: Instant,
}
#[derive(Debug, PartialEq, Eq)]
pub enum GrantError {
    RandomnessUnavailable,
    InvalidLifetime,
    InvalidRoot,
    GrantLimit,
}

impl GrantStore {
    pub fn new(lifetime: Duration) -> Result<Self, GrantError> {
        if lifetime.is_zero() || Instant::now().checked_add(lifetime).is_none() {
            return Err(GrantError::InvalidLifetime);
        }
        Ok(Self {
            grants: Arc::new(Mutex::new(HashMap::new())),
            lifetime,
        })
    }
    pub fn issue(&self, root: impl Into<PathBuf>) -> Result<String, GrantError> {
        let root = root
            .into()
            .canonicalize()
            .map_err(|_| GrantError::InvalidRoot)?;
        if !root.is_dir() {
            return Err(GrantError::InvalidRoot);
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|_| GrantError::RandomnessUnavailable)?;
        let token = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut grants = self.grants.lock().expect("grant lock poisoned");
        let now = Instant::now();
        grants.retain(|_, grant| grant.expires > now);
        if grants.len() >= MAX_GRANTS {
            return Err(GrantError::GrantLimit);
        }
        let expires = now
            .checked_add(self.lifetime)
            .ok_or(GrantError::InvalidLifetime)?;
        grants.insert(token.clone(), Grant { root, expires });
        Ok(token)
    }
    pub fn revoke(&self, token: &str) -> bool {
        self.grants
            .lock()
            .expect("grant lock poisoned")
            .remove(token)
            .is_some()
    }
    fn resolve(&self, token: &str, relative: &str) -> Option<PathBuf> {
        let valid_name = relative == "live.m3u8"
            || relative == "master.m3u8"
            || relative == "audio.m3u8"
            || valid_segment(relative, "segment")
            || valid_segment(relative, "audio");
        if !valid_name {
            return None;
        }
        let mut grants = self.grants.lock().expect("grant lock poisoned");
        let grant = grants.get(token)?;
        if grant.expires <= Instant::now() {
            grants.remove(token);
            return None;
        }
        let path = grant.root.join(relative).canonicalize().ok()?;
        path.starts_with(&grant.root).then_some(path)
    }
}

fn valid_segment(name: &str, prefix: &str) -> bool {
    let Some(value) = name.strip_prefix(prefix) else {
        return false;
    };
    value.len() == 9
        && value.ends_with(".ts")
        && value.as_bytes()[..6].iter().all(u8::is_ascii_digit)
}

fn cors(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn media(
    State(grants): State<GrantStore>,
    AxumPath((token, file)): AxumPath<(String, String)>,
) -> Response {
    let Some(path) = grants.resolve(&token, &file) else {
        return cors((StatusCode::NOT_FOUND, "not found\n").into_response());
    };
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_MEDIA_BYTES => metadata,
        _ => return cors((StatusCode::NOT_FOUND, "not found\n").into_response()),
    };
    let body = match tokio::fs::File::open(&path).await {
        Ok(file) => {
            let mut body = Vec::new();
            if file
                .take(MAX_MEDIA_BYTES + 1)
                .read_to_end(&mut body)
                .await
                .is_ok()
                && body.len() as u64 == metadata.len()
            {
                body
            } else {
                return cors((StatusCode::NOT_FOUND, "not found\n").into_response());
            }
        }
        _ => return cors((StatusCode::NOT_FOUND, "not found\n").into_response()),
    };
    let content_type = if file.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp2t"
    };
    cors(([(header::CONTENT_TYPE, content_type)], Body::from(body)).into_response())
}
async fn revoke(State(grants): State<GrantStore>, AxumPath(token): AxumPath<String>) -> Response {
    cors(
        (if grants.revoke(&token) {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::NOT_FOUND
        })
        .into_response(),
    )
}
async fn preflight() -> Response {
    cors(
        (
            StatusCode::NO_CONTENT,
            [
                (
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    "GET, HEAD, DELETE, OPTIONS",
                ),
                (header::ACCESS_CONTROL_ALLOW_HEADERS, "Range"),
            ],
        )
            .into_response(),
    )
}
pub fn app(grants: GrantStore) -> Router {
    Router::new()
        .route("/cast/:grant/:file", get(media).options(preflight))
        .route("/cast/:grant", delete(revoke).options(preflight))
        .with_state(grants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Method};
    use tower::ServiceExt;
    #[tokio::test]
    async fn serves_manifest_segment_and_revoke() {
        let root = std::env::temp_dir().join(format!("ozma-cast-media-{}", std::process::id()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("live.m3u8"), "#EXTM3U\nsegment000001.ts\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("segment000001.ts"), b"segment")
            .await
            .unwrap();
        let store = GrantStore::new(Duration::from_secs(30)).unwrap();
        let token = store.issue(&root).unwrap();
        let router = app(store);
        let request = |method, uri| {
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        };
        let preflight = router
            .clone()
            .oneshot(request(Method::OPTIONS, format!("/cast/{token}")))
            .await
            .unwrap();
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert!(preflight.headers()[header::ACCESS_CONTROL_ALLOW_METHODS]
            .to_str()
            .unwrap()
            .contains("DELETE"));
        let response = router
            .clone()
            .oneshot(request(Method::GET, format!("/cast/{token}/live.m3u8")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
            b"#EXTM3U\nsegment000001.ts\n"
        );
        assert_eq!(
            router
                .clone()
                .oneshot(request(
                    Method::GET,
                    format!("/cast/{token}/segment000001.ts")
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            router
                .clone()
                .oneshot(request(Method::DELETE, format!("/cast/{token}")))
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );
        for file in ["live.m3u8", "segment000001.ts"] {
            assert_eq!(
                router
                    .clone()
                    .oneshot(request(Method::GET, format!("/cast/{token}/{file}")))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}

#[derive(clap::Parser)]
struct Args {
    /// Private directory written by cast_hls.
    root: PathBuf,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// Playback grant lifetime in seconds.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=3600))]
    ttl: u64,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    use clap::Parser;
    let args = Args::parse();
    let grants = GrantStore::new(Duration::from_secs(args.ttl))
        .map_err(|error| std::io::Error::other(format!("invalid grant lifetime: {error:?}")))?;
    let token = grants
        .issue(&args.root)
        .map_err(|error| std::io::Error::other(format!("could not issue grant: {error:?}")))?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await?;
    let origin = listener.local_addr()?;
    println!("Video playlist: http://{origin}/cast/{token}/live.m3u8");
    println!("Audio/video playlist: http://{origin}/cast/{token}/master.m3u8");
    axum::serve(listener, app(grants)).await
}
