use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::RwLock;

const ACCOUNTS_FILENAME: &str = "accounts.json";

/// Account status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum AccountStatus {
    Active,
    Inactive,
    Error,
    ReAuth,
}

impl std::fmt::Display for AccountStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Inactive => write!(f, "inactive"),
            Self::Error => write!(f, "error"),
            Self::ReAuth => write!(f, "re-auth"),
        }
    }
}

/// A Copilot account with tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotAccount {
    pub id: String,
    pub label: String,
    pub github_token: String,
    pub copilot_token: Option<String>,
    pub copilot_token_expiry: Option<u64>,
    pub account_type: String,
    pub status: AccountStatus,
    pub last_used: Option<u64>,
    pub error_count: u32,
    pub created_at: u64,
}

/// An API key entry
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyEntry {
    pub key: String,
    pub label: String,
    pub created_at: u64,
    pub last_used: Option<u64>,
    pub request_count: u64,
}

/// Persisted accounts data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsData {
    pub accounts: Vec<CopilotAccount>,
    pub rotation_index: usize,
    pub api_keys: Vec<ApiKeyEntry>,
}

impl Default for AccountsData {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            rotation_index: 0,
            api_keys: Vec::new(),
        }
    }
}

/// Thread-safe account store with file persistence
pub struct AccountStore {
    data: RwLock<AccountsData>,
    file_path: PathBuf,
}

impl AccountStore {
    pub fn new(app_dir: PathBuf) -> Self {
        Self {
            data: RwLock::new(AccountsData::default()),
            file_path: app_dir.join(ACCOUNTS_FILENAME),
        }
    }

    /// Load accounts from disk
    pub async fn load(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.file_path.exists() {
            let content = tokio::fs::read_to_string(&self.file_path).await?;
            let mut data: AccountsData = serde_json::from_str(&content)?;
            // Ensure defaults
            if data.api_keys.is_empty() {
                data.api_keys = Vec::new();
            }
            *self.data.write().await = data;
        }
        Ok(())
    }

    /// Save accounts to disk atomically
    pub async fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let data = self.data.read().await;
        let json = serde_json::to_string_pretty(&*data)?;

        // Atomic write: write to temp, then rename
        let tmp_path = self.file_path.with_extension("json.tmp");
        if let Some(parent) = self.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&tmp_path, &json).await?;
        tokio::fs::rename(&tmp_path, &self.file_path).await?;
        Ok(())
    }

    // ── Account CRUD ──────────────────────────────────────────────────────

    pub async fn get_all_accounts(&self) -> Vec<CopilotAccount> {
        self.data.read().await.accounts.clone()
    }

    pub async fn get_active_accounts(&self) -> Vec<CopilotAccount> {
        self.data
            .read()
            .await
            .accounts
            .iter()
            .filter(|a| a.status == AccountStatus::Active && a.copilot_token.is_some())
            .cloned()
            .collect()
    }

    pub async fn get_account(&self, id: &str) -> Option<CopilotAccount> {
        self.data
            .read()
            .await
            .accounts
            .iter()
            .find(|a| a.id == id)
            .cloned()
    }

    pub async fn add_account(&self, label: String, github_token: String, account_type: String) -> CopilotAccount {
        let account = CopilotAccount {
            id: uuid::Uuid::new_v4().to_string(),
            label,
            github_token,
            copilot_token: None,
            copilot_token_expiry: None,
            account_type,
            status: AccountStatus::Inactive,
            last_used: None,
            error_count: 0,
            created_at: now_ms(),
        };
        let mut data = self.data.write().await;
        data.accounts.push(account.clone());
        drop(data);
        let _ = self.save().await;
        account
    }

    pub async fn update_account(&self, updated: CopilotAccount) {
        let mut data = self.data.write().await;
        if let Some(acc) = data.accounts.iter_mut().find(|a| a.id == updated.id) {
            *acc = updated;
        }
        drop(data);
        let _ = self.save().await;
    }

    pub async fn remove_account(&self, id: &str) -> bool {
        let mut data = self.data.write().await;
        let before = data.accounts.len();
        data.accounts.retain(|a| a.id != id);
        let removed = data.accounts.len() < before;
        drop(data);
        if removed {
            let _ = self.save().await;
        }
        removed
    }

    // ── Rotation ──────────────────────────────────────────────────────────

    pub async fn get_next_rotation_account(&self) -> Option<CopilotAccount> {
        let mut data = self.data.write().await;
        let active: Vec<usize> = data
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.status == AccountStatus::Active && a.copilot_token.is_some())
            .map(|(i, _)| i)
            .collect();

        if active.is_empty() {
            return None;
        }

        let idx = data.rotation_index % active.len();
        data.rotation_index = (data.rotation_index + 1) % active.len();
        Some(data.accounts[active[idx]].clone())
    }

    pub async fn mark_account_used(&self, id: &str) {
        let mut data = self.data.write().await;
        if let Some(acc) = data.accounts.iter_mut().find(|a| a.id == id) {
            acc.last_used = Some(now_ms());
        }
    }

    #[allow(dead_code)]
    pub async fn mark_account_error(&self, id: &str) {
        let mut data = self.data.write().await;
        if let Some(acc) = data.accounts.iter_mut().find(|a| a.id == id) {
            acc.error_count += 1;
            if acc.error_count >= 3 {
                acc.status = AccountStatus::Error;
            }
        }
        drop(data);
        let _ = self.save().await;
    }

    // ── API Keys ──────────────────────────────────────────────────────────

    pub async fn get_api_keys(&self) -> Vec<ApiKeyEntry> {
        self.data.read().await.api_keys.clone()
    }

    pub async fn add_api_key(&self, label: String) -> ApiKeyEntry {
        let key_str = format!(
            "rc-{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")
        );
        let entry = ApiKeyEntry {
            key: key_str,
            label,
            created_at: now_ms(),
            last_used: None,
            request_count: 0,
        };
        let mut data = self.data.write().await;
        data.api_keys.push(entry.clone());
        drop(data);
        let _ = self.save().await;
        entry
    }

    pub async fn remove_api_key(&self, key: &str) -> bool {
        let mut data = self.data.write().await;
        let before = data.api_keys.len();
        data.api_keys.retain(|k| k.key != key);
        let removed = data.api_keys.len() < before;
        drop(data);
        if removed {
            let _ = self.save().await;
        }
        removed
    }

    pub async fn validate_api_key(&self, key: &str) -> Option<ApiKeyEntry> {
        self.data
            .read()
            .await
            .api_keys
            .iter()
            .find(|k| k.key == key)
            .cloned()
    }

    pub async fn record_api_key_usage(&self, key: &str) {
        let mut data = self.data.write().await;
        if let Some(entry) = data.api_keys.iter_mut().find(|k| k.key == key) {
            entry.request_count += 1;
            entry.last_used = Some(now_ms());
        }
    }

    pub async fn has_api_keys(&self) -> bool {
        !self.data.read().await.api_keys.is_empty()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Get application data directory
pub fn app_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rotation-copilot")
}
