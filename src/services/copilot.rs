use serde::{Deserialize, Serialize};

use crate::state::{copilot_base_url, copilot_headers};
use crate::services::ModelsResponse;

/// Token response from Copilot API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotTokenResponse {
    pub token: String,
    pub expires_at: u64,
    pub refresh_in: u64,
}

/// Fetch Copilot API token from GitHub token
pub async fn get_copilot_token(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<CopilotTokenResponse, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("token {github_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "rotation-copilot")
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Copilot token fetch failed ({status}): {body}").into());
    }

    let token_resp: CopilotTokenResponse = resp.json().await?;
    Ok(token_resp)
}

/// Fetch available models from Copilot
pub async fn get_models(
    client: &reqwest::Client,
    copilot_token: &str,
    account_type: &str,
    vscode_version: &str,
) -> Result<ModelsResponse, Box<dyn std::error::Error + Send + Sync>> {
    let base = copilot_base_url(account_type);
    let url = format!("{base}/models");

    let headers = copilot_headers(copilot_token, vscode_version, false);
    let mut req = client.get(&url);
    for (k, v) in headers {
        req = req.header(&k, &v);
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Models fetch failed ({status}): {body}").into());
    }

    let models: ModelsResponse = resp.json().await?;
    Ok(models)
}

/// Send chat completions request to Copilot (returns raw Response for streaming)
pub async fn create_chat_completions(
    client: &reqwest::Client,
    copilot_token: &str,
    account_type: &str,
    vscode_version: &str,
    payload: &serde_json::Value,
) -> Result<reqwest::Response, Box<dyn std::error::Error + Send + Sync>> {
    let base = copilot_base_url(account_type);
    let url = format!("{base}/chat/completions");

    // Check for vision content
    let has_vision = payload["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter().any(|m| {
                m["content"].as_array().map_or(false, |parts| {
                    parts.iter().any(|p| p["type"] == "image_url")
                })
            })
        })
        .unwrap_or(false);

    // Check for agent-style conversation
    let has_agent = payload["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .any(|m| m["role"] == "assistant" || m["role"] == "tool")
        })
        .unwrap_or(false);

    let headers = copilot_headers(copilot_token, vscode_version, has_vision);
    let mut req = client.post(&url);
    for (k, v) in headers {
        req = req.header(&k, &v);
    }
    if has_agent {
        req = req.header("X-Initiator", "agent");
    }

    let resp = req
        .json(payload)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        // Detect context overflow
        if status.as_u16() == 413
            || body.contains("exceeds limit")
            || body.contains("operation timed out")
        {
            return Err(format!("400: prompt is too long for model").into());
        }

        return Err(format!("Copilot API error ({status}): {body}").into());
    }

    Ok(resp)
}

/// Send embeddings request to Copilot
pub async fn create_embeddings(
    client: &reqwest::Client,
    copilot_token: &str,
    account_type: &str,
    vscode_version: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let base = copilot_base_url(account_type);
    let url = format!("{base}/embeddings");

    let headers = copilot_headers(copilot_token, vscode_version, false);
    let mut req = client.post(&url);
    for (k, v) in headers {
        req = req.header(&k, &v);
    }

    let resp = req.json(payload).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Embeddings error ({status}): {body}").into());
    }

    Ok(resp.json().await?)
}

/// Send responses API request to Copilot
pub async fn create_responses(
    client: &reqwest::Client,
    copilot_token: &str,
    account_type: &str,
    vscode_version: &str,
    payload: &serde_json::Value,
) -> Result<reqwest::Response, Box<dyn std::error::Error + Send + Sync>> {
    let base = copilot_base_url(account_type);
    let url = format!("{base}/responses");

    let headers = copilot_headers(copilot_token, vscode_version, false);
    let mut req = client.post(&url);
    for (k, v) in headers {
        req = req.header(&k, &v);
    }

    let resp = req.json(payload).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Responses API error ({status}): {body}").into());
    }

    Ok(resp)
}
