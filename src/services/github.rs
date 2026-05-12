use serde::{Deserialize, Serialize};

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// Device code response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Start GitHub device flow
pub async fn get_device_code(
    client: &reqwest::Client,
) -> Result<DeviceCodeResponse, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "client_id": GITHUB_CLIENT_ID,
            "scope": "read:user"
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Device code request failed: {body}").into());
    }

    Ok(resp.json().await?)
}

/// Poll for access token after device flow authorization
pub async fn poll_access_token(
    client: &reqwest::Client,
    device_code: &str,
    interval: u64,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let poll_interval = std::time::Duration::from_secs(interval + 1);

    loop {
        tokio::time::sleep(poll_interval).await;

        let resp = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "client_id": GITHUB_CLIENT_ID,
                "device_code": device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
            }))
            .send()
            .await?;

        let json: serde_json::Value = resp.json().await?;

        if let Some(token) = json["access_token"].as_str() {
            return Ok(token.to_string());
        }

        let error = json["error"].as_str().unwrap_or("");
        match error {
            "authorization_pending" => continue,
            "slow_down" => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            "expired_token" => return Err("Device code expired".into()),
            "access_denied" => return Err("Access denied by user".into()),
            _ => continue,
        }
    }
}

/// Get GitHub user info
#[derive(Debug, Deserialize)]
pub struct GitHubUser {
    pub login: String,
}

pub async fn get_github_user(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<GitHubUser, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("token {github_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "rotation-copilot")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err("Failed to fetch GitHub user".into());
    }

    Ok(resp.json().await?)
}

/// Get Copilot usage/quota info
pub async fn get_copilot_usage(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get("https://api.github.com/copilot_internal/user")
        .header("Authorization", format!("token {github_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "rotation-copilot")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err("Failed to fetch Copilot usage".into());
    }

    Ok(resp.json().await?)
}
