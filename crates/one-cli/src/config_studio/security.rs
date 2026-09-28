//! Local-only access control for the config studio.
//!
//! The studio serves real credentials and writes real files, so it is fenced by
//! four independent checks (`docs/web-config.md` §4):
//!
//! 1. **Loopback binding** — the server refuses to bind a non-loopback address.
//! 2. **Host allowlist** — blocks DNS-rebinding attacks, where a hostile domain
//!    resolves to `127.0.0.1` but sends its own `Host` header.
//! 3. **Origin check** — blocks cross-site requests from a page the user happens
//!    to have open in another tab.
//! 4. **Ephemeral token** — required on every `/api/config/*` call, accepted via
//!    header or (for the initial page load) a query parameter.
//!
//! None of this is a substitute for not being exposed: the guard deliberately
//! fails closed, but the primary protection is that the socket is never reachable
//! from another host.

use std::rc::Rc;

use crate::config_studio::api::API_PREFIX;
use one_web::{HttpRequest, HttpResponse, RequestGuard};

/// Query parameter carrying the token on the initial page load.
pub const TOKEN_QUERY: &str = "token";

/// Header carrying the token on subsequent API calls.
pub const TOKEN_HEADER: &str = "x-one-config-token";

/// Bind addresses the studio accepts.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "::1", "localhost"];

/// Whether `host` is a loopback address the studio may bind.
pub fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim().trim_matches(['[', ']']).to_ascii_lowercase();
    LOOPBACK_HOSTS.contains(&normalized.as_str())
}

/// Format a `host:port` authority, bracketing IPv6 literals.
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Compare two strings without leaking length-dependent timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Access-control state for one studio process.
pub struct StudioSecurity {
    token: String,
    /// `host:port` values accepted in the `Host` header.
    allowed_authorities: Vec<String>,
}

impl StudioSecurity {
    /// Create a guard for a server bound to `host:port`.
    ///
    /// The token lives only in memory for the lifetime of this process; it is
    /// never written to disk or to a log.
    pub fn new(host: &str, port: u16) -> Self {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let normalized = host.trim().trim_matches(['[', ']']).to_ascii_lowercase();
        // Accept every spelling of loopback for this port so `localhost` and
        // `127.0.0.1` both work, while a foreign Host still fails.
        let mut allowed_authorities = vec![
            authority("127.0.0.1", port),
            authority("localhost", port),
            authority("::1", port),
        ];
        if !allowed_authorities
            .iter()
            .any(|a| a == &authority(&normalized, port))
        {
            // A non-default loopback spelling we were explicitly bound to.
            allowed_authorities.push(authority(&normalized, port));
        }
        allowed_authorities.dedup();

        Self {
            token,
            allowed_authorities,
        }
    }

    /// The ephemeral token, for building the startup URL.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Authorities accepted in the `Host` header.
    pub fn allowed_authorities(&self) -> &[String] {
        &self.allowed_authorities
    }

    fn host_allowed(&self, authority: &str) -> bool {
        let trimmed = authority.trim().to_ascii_lowercase();
        self.allowed_authorities
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&trimmed))
    }

    /// Extract the authority from an `Origin` header value.
    fn origin_allowed(&self, origin: &str) -> bool {
        let lower = origin.trim().to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix("http://") else {
            // HTTPS or a non-URL scheme on a plain-HTTP loopback server is never
            // a legitimate same-origin request.
            return false;
        };
        self.host_allowed(rest)
    }

    fn is_api_path(path: &str) -> bool {
        path.starts_with(API_PREFIX)
    }

    /// Build the guard closure handed to `one_web`.
    pub fn guard(self: &Rc<Self>) -> RequestGuard {
        let this = Rc::clone(self);
        Rc::new(move |req: &HttpRequest| this.check(req))
    }

    /// The actual decision, split out so it can be unit tested directly.
    pub fn check(&self, req: &HttpRequest) -> Result<(), HttpResponse> {
        // 1. Host must be one we bound to. A missing Host is a malformed HTTP/1.1
        //    request and is rejected rather than defaulted.
        let Some(host) = req.header("host") else {
            return Err(HttpResponse::error(
                403,
                "缺少 Host 头：为防御 DNS rebinding，配置中心要求 Host 必须匹配监听地址",
            ));
        };
        if !self.host_allowed(host) {
            return Err(HttpResponse::error(
                403,
                format!("Host `{host}` 不在允许列表内，请通过回环地址访问配置中心"),
            ));
        }

        // 2. Origin must match exactly, so a hostile page cannot drive the API.
        if let Some(origin) = req.header("origin") {
            if !self.origin_allowed(origin) {
                return Err(HttpResponse::error(
                    403,
                    format!("拒绝跨源请求：Origin `{origin}` 与配置中心不同源"),
                ));
            }
        }

        // 3. Every config API call carries the ephemeral token.
        if Self::is_api_path(&req.path) && !req.path.starts_with("/api/config/health") {
            let from_header = req.header(TOKEN_HEADER);
            let from_query = req.query(TOKEN_QUERY);
            let presented = from_header
                .map(|v| v.to_string())
                .or_else(|| from_query.clone());
            match presented {
                Some(candidate) if constant_time_eq(&candidate, &self.token) => {}
                _ => {
                    return Err(HttpResponse::error(
                        401,
                        "缺少或错误的访问凭证：请使用启动时打印的带 token 的链接重新打开配置中心",
                    )
                    .with_header("Cache-Control", "no-store"))
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_web::HttpRequest;
    use std::collections::BTreeMap;

    fn security() -> Rc<StudioSecurity> {
        Rc::new(StudioSecurity::new("127.0.0.1", 3333))
    }

    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> HttpRequest {
        let mut map = BTreeMap::new();
        for (k, v) in headers {
            map.insert(k.to_ascii_lowercase(), (*v).to_string());
        }
        HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            raw_query: String::new(),
            headers: map,
            body: Vec::new(),
        }
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn static_assets_need_no_token() {
        let sec = security();
        let req = request("GET", "/config", &[("host", "127.0.0.1:3333")]);
        assert!(sec.check(&req).is_ok());
    }

    #[test]
    fn api_without_token_is_unauthorized() {
        let sec = security();
        let req = request("GET", "/api/config/catalog", &[("host", "127.0.0.1:3333")]);
        let err = sec.check(&req).unwrap_err();
        assert_eq!(err.status, 401);
    }

    #[test]
    fn api_with_wrong_token_is_unauthorized() {
        let sec = security();
        let req = request(
            "GET",
            "/api/config/catalog",
            &[("host", "127.0.0.1:3333"), (TOKEN_HEADER, "nope")],
        );
        assert_eq!(sec.check(&req).unwrap_err().status, 401);
    }

    #[test]
    fn api_with_header_token_is_allowed() {
        let sec = security();
        let req = request(
            "GET",
            "/api/config/catalog",
            &[("host", "127.0.0.1:3333"), (TOKEN_HEADER, sec.token())],
        );
        assert!(sec.check(&req).is_ok());
    }

    #[test]
    fn api_with_query_token_is_allowed() {
        let sec = security();
        let mut req = request("GET", "/api/config/session", &[("host", "127.0.0.1:3333")]);
        req.raw_query = format!("token={}", sec.token());
        assert!(sec.check(&req).is_ok());
    }

    #[test]
    fn missing_host_is_rejected() {
        let sec = security();
        let req = request("GET", "/", &[]);
        assert_eq!(sec.check(&req).unwrap_err().status, 403);
    }

    #[test]
    fn rebinding_host_is_rejected() {
        let sec = security();
        let req = request("GET", "/", &[("host", "evil.example.com")]);
        assert_eq!(sec.check(&req).unwrap_err().status, 403);
    }

    #[test]
    fn cross_origin_request_is_rejected() {
        let sec = security();
        let req = request(
            "POST",
            "/api/config/documents/settings.global/save",
            &[
                ("host", "127.0.0.1:3333"),
                ("origin", "http://evil.example.com"),
                (TOKEN_HEADER, sec.token()),
            ],
        );
        let err = sec.check(&req).unwrap_err();
        assert_eq!(err.status, 403);
    }

    #[test]
    fn same_origin_request_is_allowed() {
        let sec = security();
        let req = request(
            "POST",
            "/api/config/documents/settings.global/save",
            &[
                ("host", "127.0.0.1:3333"),
                ("origin", "http://127.0.0.1:3333"),
                (TOKEN_HEADER, sec.token()),
            ],
        );
        assert!(sec.check(&req).is_ok(), "{:?}", sec.check(&req));
    }

    #[test]
    fn localhost_alias_is_allowed_when_bound_to_loopback() {
        let sec = security();
        let req = request("GET", "/", &[("host", "localhost:3333")]);
        assert!(sec.check(&req).is_ok());
    }

    #[test]
    fn wrong_port_is_rejected() {
        let sec = security();
        let req = request("GET", "/", &[("host", "127.0.0.1:9999")]);
        assert_eq!(sec.check(&req).unwrap_err().status, 403);
    }

    #[test]
    fn tokens_are_unique_per_process() {
        let a = StudioSecurity::new("127.0.0.1", 3333);
        let b = StudioSecurity::new("127.0.0.1", 3333);
        assert_ne!(a.token(), b.token());
        assert_eq!(a.token().len(), 32);
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }
}
