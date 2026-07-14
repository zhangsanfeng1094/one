use reqwest::Client;
use serde_json::json;

use crate::error::{Result, SessionError};

/// Upload HTML content to a GitHub Gist (public by default).
pub async fn share_to_gist(html: impl Into<String>, title: impl Into<String>) -> Result<String> {
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| SessionError::Share("GITHUB_TOKEN or GH_TOKEN not set".into()))?;

    let client = Client::new();
    let body = json!({
        "description": title.into(),
        "public": true,
        "files": {
            "session.html": {
                "content": html.into(),
            }
        }
    });

    let response = client
        .post("https://api.github.com/gists")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "one")
        .json(&body)
        .send()
        .await
        .map_err(|e| SessionError::Share(e.to_string()))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(SessionError::Share(format!("gist API {status}: {text}")));
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|e| SessionError::Share(e.to_string()))?;
    let url = payload
        .get("html_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| SessionError::Share("gist response missing html_url".into()))?;

    Ok(url.to_string())
}