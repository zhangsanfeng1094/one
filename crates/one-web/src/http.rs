//! Minimal, dependency-free HTTP/1.1 request parsing and response building.
//!
//! The One Web server deliberately avoids a framework so a released binary needs
//! no runtime beyond the OS. This module provides just enough HTTP to serve the
//! embedded SPA, the built-in `/api/info` endpoint, and the pluggable config
//! studio API (which needs request bodies and precise status codes).

use std::collections::BTreeMap;
use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Default cap on the request head (request line + headers), in bytes.
pub const DEFAULT_MAX_HEAD: usize = 64 * 1024;
/// Default cap on the request body, in bytes.
pub const DEFAULT_MAX_BODY: usize = 8 * 1024 * 1024;

/// A parsed HTTP/1.1 request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// Uppercase method, e.g. `GET`, `POST`.
    pub method: String,
    /// Percent-decoded path with no query string, e.g. `/api/config/documents`.
    pub path: String,
    /// Raw (still percent-encoded) query string, without the leading `?`.
    pub raw_query: String,
    /// Header map; keys are lowercased and values trimmed.
    pub headers: BTreeMap<String, String>,
    /// Request body bytes (empty for bodyless methods).
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Value of a header, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }

    /// Decoded query parameter, if present and valid UTF-8.
    pub fn query(&self, name: &str) -> Option<String> {
        self.query_params().remove(name)
    }

    /// All query parameters, percent-decoded. Duplicate keys keep the last value.
    pub fn query_params(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for pair in self.raw_query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            out.insert(
                percent_decode(k).unwrap_or_default(),
                percent_decode(v).unwrap_or_default(),
            );
        }
        out
    }

    /// Body interpreted as UTF-8 JSON. Returns a human-readable error on failure.
    pub fn json_body(&self) -> Result<serde_json::Value, String> {
        if self.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        let text = std::str::from_utf8(&self.body)
            .map_err(|e| format!("request body is not valid UTF-8: {e}"))?;
        serde_json::from_str(text).map_err(|e| format!("invalid JSON body: {e}"))
    }

    /// A stable identifier for logging that never echoes the token or body.
    pub fn log_target(&self) -> String {
        let base = if self.raw_query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?…", self.path)
        };
        format!("{} {}", self.method, base)
    }
}

/// HTTP response ready to be serialized to the wire.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Numeric status code.
    pub status: u16,
    /// `Content-Type` header value.
    pub content_type: String,
    /// Extra headers (e.g. `Cache-Control`, `X-One-Config-Version`).
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Build a response with an arbitrary body.
    pub fn new(status: u16, content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// JSON response from a serde value.
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
        Self::new(status, "application/json; charset=utf-8", body)
    }

    /// Plain-text response.
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            body.into().into_bytes(),
        )
    }

    /// Bodyless response.
    pub fn empty(status: u16) -> Self {
        Self::new(status, "text/plain; charset=utf-8", Vec::new())
    }

    /// Error response shaped as `{ "error": "…" }` so the UI can render it uniformly.
    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, &serde_json::json!({ "error": message.into() }))
    }

    /// Attach an extra header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Reason phrase for the status code (unknown codes fall back to `OK`).
    pub fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            411 => "Length Required",
            413 => "Content Too Large",
            415 => "Unsupported Media Type",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            _ => "OK",
        }
    }

    /// Serialize status line, headers, and body to wire bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason());
        head.push_str(&format!("Content-Type: {}\r\n", self.content_type));
        head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        head.push_str("X-Content-Type-Options: nosniff\r\n");
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");

        let mut out = Vec::with_capacity(head.len() + self.body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

/// Parse one HTTP request from `reader`.
///
/// Returns `Ok(None)` when the peer closed the connection cleanly before sending
/// anything, which the caller treats as a no-op. Header-only requests (for
/// example a WebSocket upgrade) yield an empty body.
pub async fn read_request<R>(
    reader: &mut R,
    max_head: usize,
    max_body: usize,
) -> io::Result<Option<HttpRequest>>
where
    R: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > max_head {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head exceeds limit",
            ));
        }
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed inside request head",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    if head_end > max_head {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request head exceeds limit",
        ));
    }

    let head = std::str::from_utf8(&buf[..head_end - 4])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request head is not UTF-8"))?
        .to_string();

    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?
        .to_ascii_uppercase();
    let target = parts.next().unwrap_or("/");

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    // Tolerate absolute-form request targets (`GET http://host/path`) by keeping
    // only the path component.
    let raw_path = match raw_path.find("://") {
        Some(idx) => match raw_path[idx + 3..].find('/') {
            Some(slash) => &raw_path[idx + 3 + slash..],
            None => "/",
        },
        None => raw_path,
    };
    let path = percent_decode(raw_path).unwrap_or_else(|| raw_path.to_string());

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > max_body {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body exceeds limit",
        ));
    }

    let mut body = buf[head_end..].to_vec();
    while body.len() < content_length {
        let want = content_length - body.len();
        let mut chunk = vec![0u8; want.min(8192)];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed inside request body",
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Some(HttpRequest {
        method,
        path,
        raw_query: raw_query.to_string(),
        headers,
        body,
    }))
}

/// First index of `needle` inside `haystack`.
pub fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Percent-decode a URL component. Returns `None` on malformed escapes or
/// invalid UTF-8, so callers can fall back to the raw value.
pub fn percent_decode(input: &str) -> Option<String> {
    if !input.contains('%') && !input.contains('+') {
        return Some(input.to_string());
    }
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode everything outside the unreserved set (used for query values).
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn parses_get_with_query() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(b"GET /config?token=ab%20cd&x=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let req = read_request(&mut server, DEFAULT_MAX_HEAD, DEFAULT_MAX_BODY)
            .await
            .unwrap()
            .expect("request");
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/config");
        assert_eq!(req.query("token").as_deref(), Some("ab cd"));
        assert_eq!(req.query("x").as_deref(), Some("1"));
        assert_eq!(req.header("host"), Some("127.0.0.1"));
        assert!(req.body.is_empty());
    }

    #[tokio::test]
    async fn parses_post_body_split_across_reads() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let payload = r#"{"draft":"hello"}"#;
        let head = format!(
            "POST /api/config/validate HTTP/1.1\r\nHost: h\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        client.write_all(head.as_bytes()).await.unwrap();
        client.write_all(payload.as_bytes()).await.unwrap();

        let req = read_request(&mut server, DEFAULT_MAX_HEAD, DEFAULT_MAX_BODY)
            .await
            .unwrap()
            .expect("request");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/config/validate");
        let value = req.json_body().unwrap();
        assert_eq!(value["draft"], "hello");
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        let got = read_request(&mut server, DEFAULT_MAX_HEAD, DEFAULT_MAX_BODY)
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(b"POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 999999\r\n\r\n")
            .await
            .unwrap();
        let err = read_request(&mut server, DEFAULT_MAX_HEAD, 1024)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn response_serializes_head_and_body() {
        let resp = HttpResponse::json(409, &serde_json::json!({"error": "conflict"}))
            .with_header("X-One-Config-Version", "abc");
        let bytes = resp.to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 409 Conflict\r\n"));
        assert!(text.contains("X-One-Config-Version: abc\r\n"));
        assert!(text.ends_with(r#"{"error":"conflict"}"#));
    }

    #[test]
    fn percent_decode_rejects_malformed() {
        assert_eq!(percent_decode("a%2Fb").as_deref(), Some("a/b"));
        assert!(percent_decode("a%2").is_none());
    }
}
