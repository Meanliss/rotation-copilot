mod account_store;
mod routes;
mod services;
mod state;

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::Router;
use clap::{Parser, Subcommand};
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

use account_store::{app_dir, AccountStatus, AccountStore};
use routes::admin::OAuthStore;
use state::{AppState, ServerConfig};

#[derive(Parser)]
#[command(name = "rotation-copilot", version, about = "Multi-account GitHub Copilot proxy with round-robin rotation")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy server
    Start {
        /// Port to listen on
        #[arg(short, long, default_value_t = 4141)]
        port: u16,

        /// Host/IP to bind to (0.0.0.0 = all interfaces)
        #[arg(long, default_value = "0.0.0.0")]
        host: String,

        /// Account type (individual, business, enterprise)
        #[arg(long, default_value = "individual")]
        account_type: String,

        /// GitHub token (skip OAuth)
        #[arg(long)]
        github_token: Option<String>,

        /// Rate limit in seconds between requests
        #[arg(long)]
        rate_limit: Option<u64>,

        /// Wait instead of returning 429 on rate limit
        #[arg(long)]
        rate_limit_wait: bool,

        /// Verbose logging
        #[arg(short, long)]
        verbose: bool,

        /// Open browser automatically
        #[arg(long)]
        desktop: bool,
    },

    /// Authenticate with GitHub (OAuth device flow)
    Auth {
        /// Account type
        #[arg(long, default_value = "individual")]
        account_type: String,
    },

    /// Check Copilot usage/quota
    CheckUsage,

    /// Show debug info
    Debug,
}

fn main() {
    let cli = Cli::parse();

    // Default to `start --desktop` when double-clicked (no arguments)
    let command = cli.command.unwrap_or(Commands::Start {
        port: 4141,
        host: "0.0.0.0".into(),
        account_type: "individual".into(),
        github_token: None,
        rate_limit: None,
        rate_limit_wait: false,
        verbose: false,
        desktop: true,
    });

    match command {
        Commands::Start {
            port,
            host,
            account_type,
            github_token,
            rate_limit,
            rate_limit_wait,
            verbose,
            desktop,
        } => {
            // Initialize logging
            let filter = if verbose {
                "rotation_copilot=debug,tower_http=debug"
            } else {
                "rotation_copilot=info,tower_http=info"
            };
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
                .init();

            if desktop {
                // Desktop mode: tokio on background thread, native window on main thread
                let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
                let url = format!("http://127.0.0.1:{port}/admin");

                // Start server in background
                rt.spawn(async move {
                    run_server(
                        port,
                        host,
                        account_type,
                        github_token,
                        rate_limit,
                        rate_limit_wait,
                        verbose,
                    )
                    .await;
                });

                // Wait for server to bind
                for _ in 0..20 {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                        break;
                    }
                }

                // Run native window on main thread (blocks until close)
                open_native_window(&url);
                std::process::exit(0);
            } else {
                // CLI mode: standard tokio
                let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
                rt.block_on(async {
                    run_server(
                        port,
                        host,
                        account_type,
                        github_token,
                        rate_limit,
                        rate_limit_wait,
                        verbose,
                    )
                    .await;
                });
            }
        }

        Commands::Auth { account_type } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(async {
                tracing_subscriber::fmt()
                    .with_env_filter("rotation_copilot=info".parse::<EnvFilter>().unwrap())
                    .init();
                run_auth(&account_type).await;
            });
        }

        Commands::CheckUsage => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(async {
                tracing_subscriber::fmt()
                    .with_env_filter("rotation_copilot=info".parse::<EnvFilter>().unwrap())
                    .init();
                run_check_usage().await;
            });
        }

        Commands::Debug => {
            println!("rotation-copilot v{}", env!("CARGO_PKG_VERSION"));
            println!("Runtime: Rust {}", rustc_version());
            let dir = app_dir();
            println!("Data dir: {}", dir.display());
            println!("Accounts file: {}", dir.join("accounts.json").display());
            println!(
                "Accounts file exists: {}",
                dir.join("accounts.json").exists()
            );
            let token_file = dir.join("github_token");
            println!("Token file: {}", token_file.display());
            println!("Token file exists: {}", token_file.exists());
        }
    }
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".into())
        .trim()
        .to_string()
}

/// Get the machine's local network IP by connecting to a public address
fn get_local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

async fn handle_health(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    let active = state.store.get_active_accounts().await.len();
    let total = state.store.get_all_accounts().await.len();
    let has_keys = state.store.has_api_keys().await;
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "accounts": { "active": active, "total": total },
        "api_keys_required": has_keys,
    }))
}

async fn run_server(
    port: u16,
    host: String,
    account_type: String,
    github_token: Option<String>,
    rate_limit: Option<u64>,
    rate_limit_wait: bool,
    verbose: bool,
) {
    let data_dir = app_dir();
    tokio::fs::create_dir_all(&data_dir).await.ok();

    let config = ServerConfig {
        port,
        account_type: account_type.clone(),
        manual_approve: false,
        rate_limit_seconds: rate_limit,
        rate_limit_wait,
        show_token: false,
        verbose,
        single_account: false,
    };

    let store = AccountStore::new(data_dir.clone());
    if let Err(e) = store.load().await {
        tracing::warn!("Failed to load accounts: {e}");
    }

    let app_state = AppState::new(config, store);

    // Handle initial GitHub token if provided
    if let Some(token) = github_token {
        let account = app_state
            .store
            .add_account("CLI Account".into(), token.clone(), account_type.clone())
            .await;

        match services::copilot::get_copilot_token(&app_state.http_client, &token).await {
            Ok(resp) => {
                let mut updated = account;
                updated.copilot_token = Some(resp.token);
                updated.copilot_token_expiry = Some(resp.expires_at * 1000);
                updated.status = AccountStatus::Active;
                app_state.store.update_account(updated).await;
                tracing::info!("Account authenticated successfully");
            }
            Err(e) => {
                tracing::error!("Failed to get Copilot token: {e}");
            }
        }
    } else {
        // Try to read saved GitHub token from disk
        let token_path = data_dir.join("github_token");
        if token_path.exists() {
            if let Ok(token) = tokio::fs::read_to_string(&token_path).await {
                let token = token.trim().to_string();
                if !token.is_empty() {
                    // Check if account already exists
                    let accounts = app_state.store.get_all_accounts().await;
                    if accounts.is_empty() {
                        let account = app_state
                            .store
                            .add_account("Default".into(), token.clone(), account_type.clone())
                            .await;

                        match services::copilot::get_copilot_token(&app_state.http_client, &token)
                            .await
                        {
                            Ok(resp) => {
                                let mut updated = account;
                                updated.copilot_token = Some(resp.token);
                                updated.copilot_token_expiry = Some(resp.expires_at * 1000);
                                updated.status = AccountStatus::Active;
                                app_state.store.update_account(updated).await;
                                tracing::info!("Loaded saved GitHub token");
                            }
                            Err(e) => {
                                tracing::warn!("Saved token failed: {e}");
                            }
                        }
                    }
                }
            }
        }
    }

    // Initialize existing accounts (refresh tokens)
    init_account_refresh(app_state.clone()).await;

    // Cache models and VSCode version
    cache_models_and_version(app_state.clone()).await;

    // Setup OAuth store for admin
    let oauth_store = OAuthStore::new();

    // Build Axum router
    let app = build_router(app_state.clone(), oauth_store);

    // Print startup info
    let active = app_state.store.get_active_accounts().await.len();
    let total = app_state.store.get_all_accounts().await.len();
    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".into());

    println!("\n╔══════════════════════════════════════════════════╗");
    println!("║          Rotation Copilot                        ║");
    println!("╠══════════════════════════════════════════════════╣");
    println!("║  Local:      http://127.0.0.1:{:<18}║", port);
    let net_url = format!("http://{}:{}", local_ip, port);
    println!("║  Network:    {:<36}║", net_url);
    println!("║  Admin:      http://127.0.0.1:{}/admin{:>8}║", port, "");
    println!("║  Accounts:   {}/{:<33}║", active, total);
    println!("╚══════════════════════════════════════════════════╝\n");

    // Start server
    let bind_addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .expect(&format!("Failed to bind {bind_addr}"));

    tracing::info!("Listening on {bind_addr}");
    axum::serve(listener, app).await.expect("Server error");
}

fn build_router(state: AppState, oauth_store: Arc<OAuthStore>) -> Router {
    // Admin routes
    let admin = Router::new()
        .route("/", get(routes::admin::serve_dashboard))
        .route("/api/accounts", get(routes::admin::list_accounts))
        .route("/api/accounts", post(routes::admin::add_account))
        .route("/api/accounts/{id}", delete(routes::admin::remove_account))
        .route(
            "/api/accounts/{id}/re-auth",
            post(routes::admin::reauth_account),
        )
        .route("/api/device-code", post(routes::admin::start_device_code))
        .route("/api/api-keys", get(routes::admin::list_api_keys))
        .route("/api/api-keys", post(routes::admin::create_api_key))
        .route("/api/api-keys/{key}", delete(routes::admin::revoke_api_key))
        .route("/api/stats", get(routes::admin::get_stats))
        .layer(axum::Extension(oauth_store));

    // Proxy routes
    let proxy = Router::new()
        .route("/chat/completions", post(routes::proxy::handle_chat_completions))
        .route(
            "/v1/chat/completions",
            post(routes::proxy::handle_chat_completions),
        )
        .route("/v1/messages", post(routes::proxy::handle_messages))
        .route("/v1/responses", post(routes::proxy::handle_responses))
        .route("/embeddings", post(routes::proxy::handle_embeddings))
        .route("/v1/embeddings", post(routes::proxy::handle_embeddings))
        .route("/models", get(routes::proxy::handle_models))
        .route("/v1/models", get(routes::proxy::handle_models));

    // Root
    let root = get(|| async { "Rotation Copilot" });

    Router::new()
        .route("/", root)
        .route("/health", get(handle_health))
        .route("/v1/health", get(handle_health))
        .nest("/admin", admin)
        .route("/token", get(routes::proxy::handle_token))
        .route("/usage", get(routes::proxy::handle_usage))
        .merge(proxy)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn init_account_refresh(state: AppState) {
    let accounts = state.store.get_all_accounts().await;
    for account in accounts {
        if account.status == AccountStatus::Active || account.status == AccountStatus::Inactive {
            let state = state.clone();
            let account_id = account.id.clone();
            let github_token = account.github_token.clone();

            tokio::spawn(async move {
                // Refresh immediately
                match services::copilot::get_copilot_token(&state.http_client, &github_token).await
                {
                    Ok(resp) => {
                        let refresh_in = resp.refresh_in.max(300); // at least 5 minutes
                        if let Some(mut acc) = state.store.get_account(&account_id).await {
                            acc.copilot_token = Some(resp.token);
                            acc.copilot_token_expiry = Some(resp.expires_at * 1000);
                            acc.status = AccountStatus::Active;
                            acc.error_count = 0;
                            state.store.update_account(acc).await;
                        }

                        // Schedule periodic refresh
                        let refresh_secs = (refresh_in - 60).max(60);
                        let mut interval =
                            tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
                        interval.tick().await; // skip first

                        loop {
                            interval.tick().await;
                            match services::copilot::get_copilot_token(
                                &state.http_client,
                                &github_token,
                            )
                            .await
                            {
                                Ok(resp) => {
                                    if let Some(mut acc) =
                                        state.store.get_account(&account_id).await
                                    {
                                        acc.copilot_token = Some(resp.token);
                                        acc.copilot_token_expiry = Some(resp.expires_at * 1000);
                                        acc.status = AccountStatus::Active;
                                        acc.error_count = 0;
                                        state.store.update_account(acc).await;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Token refresh failed for {}: {e}",
                                        &account_id[..8]
                                    );
                                    if let Some(mut acc) =
                                        state.store.get_account(&account_id).await
                                    {
                                        acc.error_count += 1;
                                        if acc.error_count >= 3 {
                                            acc.status = AccountStatus::Error;
                                        }
                                        state.store.update_account(acc).await;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Initial token fetch failed for {}: {e}",
                            &account_id[..8]
                        );
                    }
                }
            });
        }
    }
}

async fn cache_models_and_version(state: AppState) {
    // Cache VSCode version
    let version = services::fetch_vscode_version(&state.http_client).await;
    *state.vscode_version.write().await = version;

    // Cache models (need an active account)
    let accounts = state.store.get_active_accounts().await;
    if let Some(account) = accounts.first() {
        if let Some(token) = &account.copilot_token {
            let vscode_ver = state.vscode_version.read().await.clone();
            match services::copilot::get_models(
                &state.http_client,
                token,
                &account.account_type,
                &vscode_ver,
            )
            .await
            {
                Ok(models) => {
                    tracing::info!("Cached {} models", models.data.len());
                    *state.models.write().await = Some(models);
                }
                Err(e) => {
                    tracing::warn!("Failed to cache models: {e}");
                }
            }
        }
    }
}

#[allow(dead_code)]
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

fn open_native_window(url: &str) {
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let event_loop = EventLoopBuilder::new().build();

    let window = WindowBuilder::new()
        .with_title("Rotation Copilot")
        .with_inner_size(tao::dpi::LogicalSize::new(1320.0, 880.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(900.0, 600.0))
        .build(&event_loop)
        .expect("Failed to create window");

    let _webview = WebViewBuilder::new()
        .with_url(url)
        .with_devtools(cfg!(debug_assertions))
        .build(&window)
        .expect("Failed to create webview");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

async fn run_auth(_account_type: &str) {
    let client = reqwest::Client::new();

    println!("Starting GitHub OAuth device flow...");
    let dc = match services::github::get_device_code(&client).await {
        Ok(dc) => dc,
        Err(e) => {
            eprintln!("Failed to get device code: {e}");
            return;
        }
    };

    println!("\nOpen: {}", dc.verification_uri);
    println!("Enter code: {}\n", dc.user_code);

    match services::github::poll_access_token(&client, &dc.device_code, dc.interval).await {
        Ok(token) => {
            let dir = app_dir();
            tokio::fs::create_dir_all(&dir).await.ok();
            let token_path = dir.join("github_token");
            if let Err(e) = tokio::fs::write(&token_path, &token).await {
                eprintln!("Failed to save token: {e}");
                return;
            }

            // Get username
            match services::github::get_github_user(&client, &token).await {
                Ok(user) => println!("Authenticated as: {}", user.login),
                Err(_) => println!("Token saved."),
            }

            println!("Token saved to: {}", token_path.display());
        }
        Err(e) => {
            eprintln!("Authentication failed: {e}");
        }
    }
}

async fn run_check_usage() {
    let client = reqwest::Client::new();
    let dir = app_dir();
    let token_path = dir.join("github_token");

    let token = match tokio::fs::read_to_string(&token_path).await {
        Ok(t) => t.trim().to_string(),
        Err(_) => {
            eprintln!("No saved token found. Run 'auth' first.");
            return;
        }
    };

    match services::github::get_copilot_usage(&client, &token).await {
        Ok(usage) => {
            println!("{}", serde_json::to_string_pretty(&usage).unwrap());
        }
        Err(e) => {
            eprintln!("Failed to fetch usage: {e}");
        }
    }
}
