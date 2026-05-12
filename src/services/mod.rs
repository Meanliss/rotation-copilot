pub mod copilot;
pub mod github;

use serde::{Deserialize, Serialize};

/// Model info from Copilot API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub preview: bool,
    pub model_picker_enabled: bool,
    pub capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub family: String,
    pub limits: ModelLimits,
    #[serde(rename = "type")]
    pub cap_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    pub max_context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_prompt_tokens: Option<u64>,
}

/// Response from GET /models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelInfo>,
}

/// VSCode version fetcher
pub async fn fetch_vscode_version(client: &reqwest::Client) -> String {
    let url = "https://aur.archlinux.org/rpc/v5/info?arg[]=visual-studio-code-bin";
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get(url).send(),
    )
    .await
    {
        Ok(Ok(resp)) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(ver) = json["results"][0]["Version"].as_str() {
                    // Strip -N suffix (e.g., "1.114.0-1" → "1.114.0")
                    return ver.split('-').next().unwrap_or(ver).to_string();
                }
            }
            "1.114.0".into()
        }
        _ => "1.114.0".into(),
    }
}
