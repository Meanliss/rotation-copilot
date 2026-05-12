use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::account_store::AccountStatus;
use crate::routes::ApiError;
use crate::services::copilot;
use crate::services::github;
use crate::state::AppState;

/// Pending OAuth sessions (device_code -> metadata)
pub struct OAuthStore {
    inner: RwLock<HashMap<String, OAuthSession>>,
}

struct OAuthSession {
    ts: u64,
    _account_type: String,
}

impl OAuthStore {
    pub fn new() -> Arc<Self> {
        let store = Arc::new(Self {
            inner: RwLock::new(HashMap::new()),
        });

        // Cleanup stale sessions every 5 minutes
        let cleanup = store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                let now = now_ms();
                let mut inner = cleanup.inner.write().await;
                inner.retain(|_, s| now - s.ts < 15 * 60 * 1000);
            }
        });

        store
    }
}

// ── Admin HTML Dashboard ────────────────────────────────────────────────────

pub async fn serve_dashboard() -> Html<&'static str> {
    Html(include_str!("../../admin/dashboard.html"))
}

// ── Account endpoints ───────────────────────────────────────────────────────

pub async fn list_accounts(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let accounts = state.store.get_all_accounts().await;
    let safe: Vec<serde_json::Value> = accounts
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "label": a.label,
                "accountType": a.account_type,
                "status": a.status.to_string(),
                "lastUsed": a.last_used,
                "errorCount": a.error_count,
                "createdAt": a.created_at,
            })
        })
        .collect();

    Ok(Json(safe))
}

#[derive(Deserialize)]
pub struct DeviceCodeRequest {
    #[serde(rename = "accountType")]
    account_type: Option<String>,
}

pub async fn start_device_code(
    State(state): State<AppState>,
    oauth_store: axum::Extension<Arc<OAuthStore>>,
    Json(body): Json<DeviceCodeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let account_type = body.account_type.unwrap_or_else(|| "individual".into());

    let dc = github::get_device_code(&state.http_client)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;

    oauth_store
        .inner
        .write()
        .await
        .insert(dc.device_code.clone(), OAuthSession {
            ts: now_ms(),
            _account_type: account_type,
        });

    Ok(Json(serde_json::json!({
        "device_code": dc.device_code,
        "user_code": dc.user_code,
        "verification_uri": dc.verification_uri,
        "expires_in": dc.expires_in,
        "interval": dc.interval,
    })))
}

#[derive(Deserialize)]
pub struct AddAccountRequest {
    label: Option<String>,
    #[serde(rename = "accountType")]
    account_type: Option<String>,
    #[serde(rename = "githubToken")]
    github_token: Option<String>,
    #[serde(rename = "deviceCode")]
    device_code: Option<String>,
}

pub async fn add_account(
    State(state): State<AppState>,
    oauth_store: axum::Extension<Arc<OAuthStore>>,
    Json(body): Json<AddAccountRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let github_token = if let Some(token) = body.github_token {
        token
    } else if let Some(device_code) = &body.device_code {
        let _session = oauth_store
            .inner
            .write()
            .await
            .remove(device_code)
            .ok_or_else(|| ApiError::bad_request("Device code expired or not found"))?;

        github::poll_access_token(&state.http_client, device_code, 5)
            .await
            .map_err(|e| ApiError::bad_request(&e.to_string()))?
    } else {
        return Err(ApiError::bad_request("Provide githubToken or deviceCode"));
    };

    // Fetch GitHub username
    let username = github::get_github_user(&state.http_client, &github_token)
        .await
        .map(|u| u.login)
        .unwrap_or_default();

    let label = body.label.unwrap_or_else(|| "Account".into());
    let final_label = if username.is_empty() {
        label
    } else {
        format!("{username} ({label})")
    };

    let account_type = body.account_type.unwrap_or_else(|| "individual".into());

    let account = state
        .store
        .add_account(final_label, github_token.clone(), account_type)
        .await;

    // Fetch initial Copilot token
    match copilot::get_copilot_token(&state.http_client, &github_token).await {
        Ok(token_resp) => {
            let mut updated = account.clone();
            updated.copilot_token = Some(token_resp.token);
            updated.copilot_token_expiry = Some(token_resp.expires_at * 1000);
            updated.status = AccountStatus::Active;
            state.store.update_account(updated.clone()).await;

            Ok(Json(serde_json::json!({
                "id": updated.id,
                "label": updated.label,
                "status": updated.status.to_string(),
            })))
        }
        Err(_) => Ok(Json(serde_json::json!({
            "id": account.id,
            "label": account.label,
            "status": account.status.to_string(),
            "warning": "GitHub token saved but Copilot token fetch failed. Try re-auth.",
        }))),
    }
}

pub async fn remove_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if !state.store.remove_account(&id).await {
        return Err(ApiError::not_found("Account not found"));
    }
    Ok(Json(serde_json::json!({"success": true})))
}

pub async fn reauth_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let account = state
        .store
        .get_account(&id)
        .await
        .ok_or_else(|| ApiError::not_found("Account not found"))?;

    let token_resp = copilot::get_copilot_token(&state.http_client, &account.github_token)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut updated = account;
    updated.copilot_token = Some(token_resp.token);
    updated.copilot_token_expiry = Some(token_resp.expires_at * 1000);
    updated.status = AccountStatus::Active;
    updated.error_count = 0;
    state.store.update_account(updated.clone()).await;

    Ok(Json(serde_json::json!({
        "id": updated.id,
        "status": updated.status.to_string(),
    })))
}

// ── API Key endpoints ───────────────────────────────────────────────────────

pub async fn list_api_keys(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let keys = state.store.get_api_keys().await;
    Ok(Json(keys))
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    label: Option<String>,
}

pub async fn create_api_key(
    State(state): State<AppState>,
    Json(body): Json<CreateKeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let entry = state
        .store
        .add_api_key(body.label.unwrap_or_else(|| "User Key".into()))
        .await;
    Ok(Json(entry))
}

pub async fn revoke_api_key(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if !state.store.remove_api_key(&key).await {
        return Err(ApiError::not_found("Key not found"));
    }
    Ok(Json(serde_json::json!({"success": true})))
}

// ── Stats ───────────────────────────────────────────────────────────────────

pub async fn get_stats(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let accounts = state.store.get_all_accounts().await;
    let keys = state.store.get_api_keys().await;
    let total_requests: u64 = keys.iter().map(|k| k.request_count).sum();

    let accounts_summary: Vec<serde_json::Value> = accounts
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": &a.id[..8.min(a.id.len())],
                "label": a.label,
                "status": a.status.to_string(),
                "lastUsed": a.last_used,
                "errorCount": a.error_count,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "totalAccounts": accounts.len(),
        "activeAccounts": accounts.iter().filter(|a| a.status == AccountStatus::Active).count(),
        "totalKeys": keys.len(),
        "totalRequests": total_requests,
        "accounts": accounts_summary,
    })))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
