use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::account_store::AccountStore;
use crate::services::ModelsResponse;

/// Per-request context extracted from middleware
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub copilot_token: String,
    #[allow(dead_code)]
    pub github_token: String,
    pub account_type: String,
    pub account_id: Option<String>,
}

/// Server-wide configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub port: u16,
    pub account_type: String,
    pub manual_approve: bool,
    pub rate_limit_seconds: Option<u64>,
    pub rate_limit_wait: bool,
    pub show_token: bool,
    pub verbose: bool,
    pub single_account: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 4141,
            account_type: "individual".into(),
            manual_approve: false,
            rate_limit_seconds: None,
            rate_limit_wait: false,
            show_token: false,
            verbose: false,
            single_account: false,
        }
    }
}

/// Shared application state (thread-safe)
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<ServerConfig>>,
    pub models: Arc<RwLock<Option<ModelsResponse>>>,
    pub vscode_version: Arc<RwLock<String>>,
    pub store: Arc<AccountStore>,
    pub http_client: reqwest::Client,
    pub last_request_timestamp: Arc<RwLock<Option<u64>>>,
    pub traffic_logs: Arc<RwLock<Vec<TrafficLogEntry>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficLogEntry {
    pub time: String,
    pub method: String,
    pub endpoint: String,
    pub model: String,
    pub account: String,
    pub status: u16,
    pub tokens: Option<u64>,
}

impl AppState {
    pub fn new(config: ServerConfig, store: AccountStore) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            models: Arc::new(RwLock::new(None)),
            vscode_version: Arc::new(RwLock::new("1.114.0".into())),
            store: Arc::new(store),
            http_client: reqwest::Client::new(),
            last_request_timestamp: Arc::new(RwLock::new(None)),
            traffic_logs: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn add_traffic_log(&self, entry: TrafficLogEntry) {
        let mut logs = self.traffic_logs.write().await;
        logs.push(entry);
        if logs.len() > 500 {
            let drain_to = logs.len() - 500;
            logs.drain(0..drain_to);
        }
    }
}

/// Copilot API base URL based on account type
pub fn copilot_base_url(account_type: &str) -> &'static str {
    match account_type {
        "business" => "https://api.business.githubcopilot.com",
        "enterprise" => "https://api.enterprise.githubcopilot.com",
        _ => "https://api.githubcopilot.com",
    }
}

/// Build Copilot API headers
pub fn copilot_headers(copilot_token: &str, vscode_version: &str, vision: bool) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Authorization".into(), format!("Bearer {copilot_token}")),
        ("Content-Type".into(), "application/json".into()),
        ("Copilot-Integration-Id".into(), "vscode-chat".into()),
        ("Editor-Version".into(), format!("vscode/{vscode_version}")),
        ("Editor-Plugin-Version".into(), "copilot-chat/0.43.0".into()),
        ("User-Agent".into(), "GitHubCopilotChat/0.43.0".into()),
        ("OpenAI-Intent".into(), "conversation-panel".into()),
        ("X-GitHub-Api-Version".into(), "2025-04-01".into()),
        ("X-Request-Id".into(), uuid::Uuid::new_v4().to_string()),
        (
            "X-VSCode-User-Agent-Library-Version".into(),
            "electron-fetch".into(),
        ),
    ];
    if vision {
        headers.push(("Copilot-Vision-Request".into(), "true".into()));
    }
    headers
}

/// Build GitHub API headers
#[allow(dead_code)]
pub fn github_headers(github_token: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".into(), format!("token {github_token}")),
        ("Accept".into(), "application/json".into()),
        ("User-Agent".into(), "rotation-copilot".into()),
    ]
}
