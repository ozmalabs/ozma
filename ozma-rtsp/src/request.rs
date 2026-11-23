// SPDX-License-Identifier: AGPL-3.0-only
//! RTSP/1.0 request and response framing.
//!
//! Parsing is intentionally tolerant of header order, case, unknown headers
//! (`User-Agent`), and missing optional headers. It is strict about the request
//! line, header syntax, and `Content-Length` so a malformed message can be
//! answered with `400 Bad Request` instead of desynchronising the connection.

use crate::error::RtspError;

/// Largest body this parser will accept from a client.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// An RTSP method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Options,
    Describe,
    Setup,
    Play,
    Pause,
    Teardown,
    GetParameter,
    SetParameter,
    /// A syntactically valid but unimplemented extension method.
    Unknown(String),
}

impl Method {
    /// Parse a method token. RTSP methods are case-sensitive.
    pub fn parse(token: &str) -> Self {
        match token {
            "OPTIONS" => Self::Options,
            "DESCRIBE" => Self::Describe,
            "SETUP" => Self::Setup,
            "PLAY" => Self::Play,
            "PAUSE" => Self::Pause,
            "TEARDOWN" => Self::Teardown,
            "GET_PARAMETER" => Self::GetParameter,
            "SET_PARAMETER" => Self::SetParameter,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Method token.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Options => "OPTIONS",
            Self::Describe => "DESCRIBE",
            Self::Setup => "SETUP",
            Self::Play => "PLAY",
            Self::Pause => "PAUSE",
            Self::Teardown => "TEARDOWN",
            Self::GetParameter => "GET_PARAMETER",
            Self::SetParameter => "SET_PARAMETER",
            Self::Unknown(value) => value,
        }
    }
}

/// Case-insensitive RTSP header collection.
///
/// Duplicate headers are retained. Lookups return the first value, which is
/// enough for RTSP's single-valued headers and avoids surprising clients that
/// repeat `Session` or `Transport`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    /// An empty header set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a header.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(&name))
        {
            existing.1 = value;
        } else {
            self.entries.push((name, value));
        }
    }

    /// Add a header without replacing an existing one.
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries.push((name.into(), value.into()));
    }

    /// First value for a header name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Every value for a header name, in insertion order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether a header is present.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Remove a header.
    pub fn remove(&mut self, name: &str) {
        self.entries
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    }

    /// Iterate over headers in their original order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Number of stored headers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no headers are stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Parse and validate a `Content-Length` header, if present.
    pub fn content_length(&self) -> Result<Option<usize>, RtspError> {
        let Some(value) = self.get("content-length") else {
            return Ok(None);
        };
        let length = value
            .trim()
            .parse::<usize>()
            .map_err(|_| RtspError::MalformedRequest("invalid Content-Length".to_owned()))?;
        if length > MAX_BODY_BYTES {
            return Err(RtspError::MalformedRequest(format!(
                "Content-Length {length} exceeds the {MAX_BODY_BYTES}-byte limit"
            )));
        }
        Ok(Some(length))
    }
}

/// A parsed RTSP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtspRequest {
    /// Request method.
    pub method: Method,
    /// Request URI, absolute or relative.
    pub uri: String,
    /// Protocol version token, normally `RTSP/1.0`.
    pub version: String,
    /// Request headers.
    pub headers: Headers,
    /// Request body.
    pub body: Vec<u8>,
}

impl RtspRequest {
    /// `CSeq` value exactly as the client sent it.
    pub fn cseq(&self) -> Option<&str> {
        self.headers.get("cseq")
    }

    /// `Session` identifier without the `;timeout=` parameter.
    pub fn session_id(&self) -> Option<&str> {
        self.headers.get("session").and_then(parse_session_header)
    }

    /// `Transport` header value.
    pub fn transport(&self) -> Option<&str> {
        self.headers.get("transport")
    }
}

/// Parse the identifier part of a `Session:` header (`id;timeout=30`).
pub fn parse_session_header(value: &str) -> Option<&str> {
    value
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Parse a complete RTSP request from an in-memory buffer.
///
/// The buffer must contain the entire message. The server uses a streaming
/// reader instead, but this entry point is useful for tests and proxies.
pub fn parse_request(raw: &[u8]) -> Result<RtspRequest, RtspError> {
    let (head, body) = split_message(raw)
        .ok_or_else(|| RtspError::MalformedRequest("missing header terminator".to_owned()))?;
    let head = std::str::from_utf8(head)
        .map_err(|_| RtspError::MalformedRequest("request headers are not UTF-8".to_owned()))?;
    request_from_head(head, body)
}

/// Parse a request head plus already-read body bytes.
pub(crate) fn request_from_head(head: &str, body: &[u8]) -> Result<RtspRequest, RtspError> {
    let (method, uri, version, headers) = parse_head(head)?;
    let length = headers.content_length()?.unwrap_or(body.len());
    if body.len() < length {
        return Err(RtspError::MalformedRequest(format!(
            "body is {} bytes but Content-Length is {length}",
            body.len()
        )));
    }
    Ok(RtspRequest {
        method,
        uri,
        version,
        headers,
        body: body[..length].to_vec(),
    })
}

/// Parse a request line and header block.
pub(crate) fn parse_head(head: &str) -> Result<(Method, String, String, Headers), RtspError> {
    let mut lines = head.split('\n').map(trim_line);
    let request_line = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| RtspError::MalformedRequest("empty request line".to_owned()))?;

    let mut parts = request_line.split_whitespace();
    let method_token = parts
        .next()
        .ok_or_else(|| RtspError::MalformedRequest("missing method".to_owned()))?;
    let uri = parts
        .next()
        .ok_or_else(|| RtspError::MalformedRequest("missing request URI".to_owned()))?;
    let version = parts
        .next()
        .ok_or_else(|| RtspError::MalformedRequest("missing RTSP version".to_owned()))?;
    if parts.next().is_some() {
        return Err(RtspError::MalformedRequest(
            "request line has trailing tokens".to_owned(),
        ));
    }
    if !version.starts_with("RTSP/") {
        return Err(RtspError::MalformedRequest(format!(
            "unsupported protocol version `{version}`"
        )));
    }

    let mut headers = Headers::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(RtspError::MalformedRequest(format!(
                "header line without a colon: `{line}`"
            )));
        };
        let name = name.trim();
        if name.is_empty() || name.chars().any(|ch| ch.is_control() || ch == ' ') {
            return Err(RtspError::MalformedRequest(
                "invalid header name".to_owned(),
            ));
        }
        headers.append(name, value.trim());
    }

    Ok((
        Method::parse(method_token),
        uri.to_owned(),
        version.to_owned(),
        headers,
    ))
}

fn trim_line(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn split_message(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(index) = find(raw, b"\r\n\r\n") {
        return Some((&raw[..index], &raw[index + 4..]));
    }
    find(raw, b"\n\n").map(|index| (&raw[..index], &raw[index + 2..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A parsed RTSP response body builder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtspResponse {
    /// Numeric status.
    pub status: u16,
    /// Reason phrase.
    pub reason: String,
    /// Response headers.
    pub headers: Headers,
    /// Response body.
    pub body: Vec<u8>,
}

impl RtspResponse {
    /// Build a response with an explicit status and reason.
    pub fn new(status: u16, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    /// `200 OK`.
    pub fn ok() -> Self {
        Self::new(200, "OK")
    }

    /// Set a header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Set the body.
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Encode the response as RTSP/1.0 bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128 + self.body.len());
        out.extend_from_slice(b"RTSP/1.0 ");
        out.extend_from_slice(self.status.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.reason.as_bytes());
        out.extend_from_slice(b"\r\n");
        for (name, value) in self.headers.iter() {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        if !self.body.is_empty() && !self.headers.contains("content-length") {
            out.extend_from_slice(b"Content-Length: ");
            out.extend_from_slice(self.body.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_with_binary_body_and_duplicate_headers() {
        let raw = b"SET_PARAMETER rtsp://host/trackID=0 RTSP/1.0\r\n\
CSeq: 12\r\n\
User-Agent: Hestia/1.0\r\n\
X-Dup: one\r\n\
X-Dup: two\r\n\
Content-Type: text/parameters\r\n\
Content-Length: 4\r\n\
\r\n\
abcd";
        let request = parse_request(raw).unwrap();
        assert_eq!(request.method, Method::SetParameter);
        assert_eq!(request.uri, "rtsp://host/trackID=0");
        assert_eq!(request.version, "RTSP/1.0");
        assert_eq!(request.cseq(), Some("12"));
        assert_eq!(request.headers.get_all("x-dup").count(), 2);
        assert_eq!(request.body, b"abcd");
    }

    #[test]
    fn tolerates_lf_only_and_unknown_headers() {
        let raw = b"OPTIONS * RTSP/1.0\nCSeq: 1\nX-Unknown: yes\n\n";
        let request = parse_request(raw).unwrap();
        assert_eq!(request.method, Method::Options);
        assert_eq!(request.cseq(), Some("1"));
    }

    #[test]
    fn parses_session_parameters() {
        assert_eq!(parse_session_header("abc123;timeout=30"), Some("abc123"));
        assert_eq!(parse_session_header(" abc123 "), Some("abc123"));
        assert_eq!(parse_session_header(";timeout=30"), None);
    }

    #[test]
    fn rejects_bad_request_lines_and_headers() {
        assert!(parse_request(b"OPTIONS\r\n\r\n").is_err());
        assert!(parse_request(b"OPTIONS * HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request(b"OPTIONS * RTSP/1.0\r\nBadHeader\r\n\r\n").is_err());
        assert!(parse_request(b"OPTIONS * RTSP/1.0\r\nCSeq: nope\r\n\r\n").is_ok());
        assert!(parse_request(b"OPTIONS * RTSP/1.0\r\nContent-Length: x\r\n\r\n").is_err());
    }

    #[test]
    fn encodes_response_with_content_length() {
        let response = RtspResponse::ok()
            .with_header("CSeq", "3")
            .with_header("Content-Type", "application/sdp")
            .with_body(b"v=0\r\n");
        let encoded = String::from_utf8(response.encode()).unwrap();
        assert!(encoded.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(encoded.contains("CSeq: 3\r\n"));
        assert!(encoded.contains("Content-Length: 5\r\n"));
        assert!(encoded.ends_with("\r\n\r\nv=0\r\n"));
    }
}
