# Rotation Copilot

Multi-account GitHub Copilot proxy with round-robin rotation. Exposes OpenAI and Anthropic compatible API endpoints.

Single binary, ~3 MB. No dependencies.

## Features

- **Multi-account rotation** — Add multiple GitHub accounts, requests are distributed round-robin across active accounts
- **OpenAI compatible** — `POST /v1/chat/completions`, `GET /v1/models`, `POST /v1/embeddings`
- **Anthropic compatible** — `POST /v1/messages` (auto-translates to/from OpenAI format)
- **Responses API** — `POST /v1/responses`
- **Streaming** — Full SSE streaming support for all endpoints
- **API keys** — Generate `rc-*` keys to protect your proxy, track usage per key
- **Admin dashboard** — Built-in web UI at `/admin` for account management, API keys, traffic logs, and settings
- **Native desktop mode** — `--desktop` flag opens the admin dashboard in a native window
- **Auto token refresh** — Copilot tokens are automatically refreshed before expiry
- **Network accessible** — Binds to `0.0.0.0` by default, other machines can connect via LAN IP
- **Health check** — `GET /health` endpoint for connectivity testing
- **Rate limiting** — Optional per-request rate limiting with wait or reject mode

## Download

Grab the latest `rotation-copilot.exe` from [Releases](../../releases).

## Usage

```bash
# Start the proxy server
rotation-copilot start

# Start on a custom port
rotation-copilot start --port 8080

# Start in desktop mode (opens native window)
rotation-copilot start --desktop

# Start with a GitHub token directly (skip OAuth)
rotation-copilot start --github-token ghp_xxxx

# Enable rate limiting (1 request per 2 seconds)
rotation-copilot start --rate-limit 2

# Rate limit with wait mode (queue instead of 429)
rotation-copilot start --rate-limit 2 --rate-limit-wait

# Bind to specific interface
rotation-copilot start --host 192.168.1.100

# Bind to localhost only (no network access)
rotation-copilot start --host 127.0.0.1

# Verbose logging
rotation-copilot start --verbose

# Authenticate via GitHub OAuth device flow
rotation-copilot auth

# Check Copilot usage/quota
rotation-copilot check-usage

# Show debug info
rotation-copilot debug
```

## API Endpoints

### OpenAI Compatible

```bash
# Chat completions
curl http://localhost:4141/v1/chat/completions \
  -H "Authorization: Bearer rc-yourkey" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'

# Streaming
curl http://localhost:4141/v1/chat/completions \
  -H "Authorization: Bearer rc-yourkey" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true
  }'

# List models
curl http://localhost:4141/v1/models \
  -H "Authorization: Bearer rc-yourkey"

# Embeddings
curl http://localhost:4141/v1/embeddings \
  -H "Authorization: Bearer rc-yourkey" \
  -H "Content-Type: application/json" \
  -d '{"model": "text-embedding-ada-002", "input": "Hello world"}'
```

### Anthropic Compatible

```bash
# Messages (auto-translates to OpenAI internally)
curl http://localhost:4141/v1/messages \
  -H "Authorization: Bearer rc-yourkey" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-3.5-sonnet",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### Admin API

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/admin` | Admin dashboard (web UI) |
| `GET` | `/admin/api/accounts` | List all accounts |
| `POST` | `/admin/api/accounts` | Add account (token or OAuth) |
| `DELETE` | `/admin/api/accounts/{id}` | Remove account |
| `POST` | `/admin/api/accounts/{id}/re-auth` | Refresh account token |
| `POST` | `/admin/api/device-code` | Start OAuth device flow |
| `GET` | `/admin/api/api-keys` | List API keys |
| `POST` | `/admin/api/api-keys` | Create API key |
| `DELETE` | `/admin/api/api-keys/{key}` | Revoke API key |
| `GET` | `/admin/api/stats` | Server statistics |
| `GET` | `/health` | Health check (status, version, accounts) |
| `GET` | `/v1/health` | Health check (alias) |
| `GET` | `/token` | Debug: get active Copilot token |
| `GET` | `/usage` | Get Copilot usage/quota |

## Configuration

All data is stored in:
- **Windows**: `%LOCALAPPDATA%\rotation-copilot\`
- **macOS**: `~/Library/Application Support/rotation-copilot/`
- **Linux**: `~/.local/share/rotation-copilot/`

Files:
- `accounts.json` — Accounts, API keys, and settings

## Network Access (Connect from Other Machines)

The server binds to `0.0.0.0` by default, making it accessible from any machine on your network.

On startup, the banner shows your LAN IP:
```
╔══════════════════════════════════════════════════╗
║          Rotation Copilot                        ║
╠══════════════════════════════════════════════════╣
║  Local:      http://127.0.0.1:4141              ║
║  Network:    http://192.168.1.100:4141           ║
║  Admin:      http://127.0.0.1:4141/admin        ║
╚══════════════════════════════════════════════════╝
```

From another machine, test connectivity:
```bash
curl http://192.168.1.100:4141/health
# {"status":"ok","version":"1.0.0","accounts":{"active":1,"total":1},"api_keys_required":true}
```

Then use the network IP as your API base:
```bash
curl http://192.168.1.100:4141/v1/chat/completions \
  -H "Authorization: Bearer rc-yourkey" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "Hello!"}]}'
```

> **Tip**: If you have API keys configured, remote machines need a valid `rc-*` key in the `Authorization` header.

## Account Types

```bash
# Individual GitHub Copilot
rotation-copilot start --account-type individual

# GitHub Copilot Business
rotation-copilot start --account-type business

# GitHub Copilot Enterprise
rotation-copilot start --account-type enterprise
```

## Use with AI Tools

### Continue.dev
```json
{
  "models": [{
    "title": "Copilot via Rotation",
    "provider": "openai",
    "model": "gpt-4o",
    "apiBase": "http://localhost:4141/v1",
    "apiKey": "rc-yourkey"
  }]
}
```

### OpenAI Python SDK
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:4141/v1",
    api_key="rc-yourkey"
)

response = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}]
)
```

### Anthropic Python SDK
```python
import anthropic

client = anthropic.Anthropic(
    base_url="http://localhost:4141",
    api_key="rc-yourkey"
)

message = client.messages.create(
    model="claude-3.5-sonnet",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello!"}]
)
```

## Build from Source

```bash
# Requires Rust 1.70+
cargo build --release

# Binary will be at target/release/rotation-copilot(.exe)
```

## License

MIT
