use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteModel {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelObject>,
}

#[derive(Debug, Deserialize)]
struct ModelObject {
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

pub async fn list_openai_compatible_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<RemoteModel>, String> {
    let base = base_url.trim();
    if base.is_empty() {
        return Err("base_url is empty; set provider.base_url first".into());
    }
    let url = format!("{}/models", base.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    if let Some(key) = api_key.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.bearer_auth(key);
    }

    let response = req
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("GET {url} failed reading response: {e}"))?;
    if !status.is_success() {
        let auth_hint = if status.as_u16() == 401
            && api_key.map(str::trim).filter(|s| !s.is_empty()).is_none()
        {
            " (provider has no api_key configured)"
        } else {
            ""
        };
        return Err(format!(
            "GET {url} returned {}{auth_hint}: {}",
            status.as_u16(),
            truncate(&text, 240)
        ));
    }

    let parsed: ModelsResponse = serde_json::from_str(&text).map_err(|e| {
        format!(
            "GET {url} returned unsupported JSON: expected {{\"data\":[{{\"id\":\"...\"}}]}} ({e})"
        )
    })?;
    let mut models = Vec::new();
    for object in parsed.data {
        let Some(id) = object.id.map(|s| s.trim().to_string()) else {
            return Err(format!(
                "GET {url} returned unsupported JSON: every data item must include string id"
            ));
        };
        if id.is_empty() {
            return Err(format!(
                "GET {url} returned unsupported JSON: every data item must include non-empty id"
            ));
        }
        models.push(RemoteModel {
            id,
            name: object.name,
        });
    }
    Ok(models)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::list_openai_compatible_models;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_once(status: &str, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        let status = status.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let n = socket.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("GET /v1/models "), "{req}");
            if req.contains("sk-test") {
                assert!(
                    req.to_ascii_lowercase()
                        .contains("authorization: bearer sk-test"),
                    "{req}"
                );
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn lists_openai_compatible_model_ids() {
        let base = serve_once("200 OK", r#"{"data":[{"id":"gpt-4.1"},{"id":"o3"}]}"#).await;

        let models = list_openai_compatible_models(&base, Some("sk-test"))
            .await
            .unwrap();

        let ids: Vec<_> = models.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["gpt-4.1", "o3"]);
    }

    #[tokio::test]
    async fn missing_data_shape_is_clear_error() {
        let base = serve_once("200 OK", r#"{"models":[{"name":"no-id"}]}"#).await;

        let err = list_openai_compatible_models(&base, None)
            .await
            .unwrap_err();

        assert!(err.contains("data"), "{err}");
        assert!(err.contains("id"), "{err}");
    }

    #[tokio::test]
    async fn unauthorized_without_key_mentions_api_key() {
        let base = serve_once("401 Unauthorized", r#"{"error":"missing key"}"#).await;

        let err = list_openai_compatible_models(&base, None)
            .await
            .unwrap_err();

        assert!(err.contains("401"), "{err}");
        assert!(err.contains("api_key"), "{err}");
    }
}
