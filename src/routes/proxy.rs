use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::StreamExt;

use crate::routes::ApiError;
use crate::state::{AppState, RequestContext, TrafficLogEntry};

/// Extract request context from headers (API key validation + account rotation)
pub async fn extract_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<RequestContext, ApiError> {
    // 1. Validate API key if keys exist
    if state.store.has_api_keys().await {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let key = auth.strip_prefix("Bearer ").unwrap_or("");
        if key.is_empty() || state.store.validate_api_key(key).await.is_none() {
            return Err(ApiError::unauthorized());
        }
        state.store.record_api_key_usage(key).await;
    }

    // 2. Rate limiting
    {
        let config = state.config.read().await;
        if let Some(limit_secs) = config.rate_limit_seconds {
            let mut last_ts = state.last_request_timestamp.write().await;
            if let Some(prev) = *last_ts {
                let now = now_ms();
                let elapsed_secs = (now - prev) / 1000;
                if elapsed_secs < limit_secs {
                    if config.rate_limit_wait {
                        let wait = (limit_secs - elapsed_secs) * 1000;
                        drop(last_ts);
                        drop(config);
                        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                        let mut last_ts = state.last_request_timestamp.write().await;
                        *last_ts = Some(now_ms());
                    } else {
                        return Err(ApiError::rate_limited());
                    }
                } else {
                    *last_ts = Some(now);
                }
            } else {
                *last_ts = Some(now_ms());
            }
        }
    }

    // 3. Get next account (round-robin)
    let account = state
        .store
        .get_next_rotation_account()
        .await
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "No active accounts available"))?;

    let _config = state.config.read().await;
    Ok(RequestContext {
        copilot_token: account.copilot_token.unwrap_or_default(),
        github_token: account.github_token.clone(),
        account_type: account.account_type.clone(),
        account_id: Some(account.id.clone()),
    })
}

// ── Chat Completions ────────────────────────────────────────────────────────

pub async fn handle_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<Response, ApiError> {
    let ctx = extract_context(&state, &headers).await?;
    let vscode_ver = state.vscode_version.read().await.clone();

    let is_stream = payload["stream"].as_bool().unwrap_or(false);

    // Forward to Copilot
    let resp = crate::services::copilot::create_chat_completions(
        &state.http_client,
        &ctx.copilot_token,
        &ctx.account_type,
        &vscode_ver,
        &payload,
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.starts_with("400:") {
            ApiError::bad_request(&msg[5..])
        } else {
            ApiError::new(StatusCode::BAD_GATEWAY, msg)
        }
    })?;

    // Mark account used
    if let Some(id) = &ctx.account_id {
        state.store.mark_account_used(id).await;
    }

    // Log traffic
    let model = payload["model"].as_str().unwrap_or("-").to_string();
    state
        .add_traffic_log(TrafficLogEntry {
            time: chrono::Utc::now().format("%H:%M:%S").to_string(),
            method: "POST".into(),
            endpoint: "/v1/chat/completions".into(),
            model,
            account: ctx.account_id.as_deref().map(|s| s[..8.min(s.len())].to_string()).unwrap_or_default(),
            status: 200,
            tokens: None,
        })
        .await;

    if is_stream {
        // Stream SSE response
        let stream = resp.bytes_stream().map(|chunk| {
            chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
        Ok(Response::builder()
            .status(200)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(Body::from_stream(stream))
            .unwrap())
    } else {
        // Non-streaming JSON response
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
        Ok(Json(body).into_response())
    }
}

// ── Messages (Anthropic-compatible) ─────────────────────────────────────────

pub async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<Response, ApiError> {
    let ctx = extract_context(&state, &headers).await?;
    let vscode_ver = state.vscode_version.read().await.clone();

    let is_stream = payload["stream"].as_bool().unwrap_or(false);
    let client_model = payload["model"].as_str().unwrap_or("").to_string();

    // Translate Anthropic → OpenAI format
    let openai_payload = translate_anthropic_to_openai(&payload);

    // Forward to Copilot
    let resp = crate::services::copilot::create_chat_completions(
        &state.http_client,
        &ctx.copilot_token,
        &ctx.account_type,
        &vscode_ver,
        &openai_payload,
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.starts_with("400:") {
            ApiError::bad_request(&msg[5..])
        } else {
            ApiError::new(StatusCode::BAD_GATEWAY, msg)
        }
    })?;

    // Mark account used
    if let Some(id) = &ctx.account_id {
        state.store.mark_account_used(id).await;
    }

    // Log traffic
    state
        .add_traffic_log(TrafficLogEntry {
            time: chrono::Utc::now().format("%H:%M:%S").to_string(),
            method: "POST".into(),
            endpoint: "/v1/messages".into(),
            model: client_model.clone(),
            account: ctx.account_id.as_deref().map(|s| s[..8.min(s.len())].to_string()).unwrap_or_default(),
            status: 200,
            tokens: None,
        })
        .await;

    if is_stream {
        // Stream SSE — pass through OpenAI chunks, translate to Anthropic events
        let byte_stream = resp.bytes_stream();
        let anthropic_stream = translate_stream_to_anthropic(byte_stream, &client_model);
        Ok(Response::builder()
            .status(200)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(Body::from_stream(anthropic_stream))
            .unwrap())
    } else {
        // Non-streaming
        let openai_response: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;

        let anthropic_response = translate_openai_to_anthropic(&openai_response, &client_model);
        Ok(Json(anthropic_response).into_response())
    }
}

// ── Responses API ───────────────────────────────────────────────────────────

pub async fn handle_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<Response, ApiError> {
    let ctx = extract_context(&state, &headers).await?;
    let vscode_ver = state.vscode_version.read().await.clone();

    let is_stream = payload["stream"].as_bool().unwrap_or(false);

    let resp = crate::services::copilot::create_responses(
        &state.http_client,
        &ctx.copilot_token,
        &ctx.account_type,
        &vscode_ver,
        &payload,
    )
    .await
    .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;

    if let Some(id) = &ctx.account_id {
        state.store.mark_account_used(id).await;
    }

    if is_stream {
        let stream = resp.bytes_stream().map(|chunk| {
            chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
        Ok(Response::builder()
            .status(200)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .body(Body::from_stream(stream))
            .unwrap())
    } else {
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
        Ok(Json(body).into_response())
    }
}

// ── Embeddings ──────────────────────────────────────────────────────────────

pub async fn handle_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = extract_context(&state, &headers).await?;
    let vscode_ver = state.vscode_version.read().await.clone();

    let result = crate::services::copilot::create_embeddings(
        &state.http_client,
        &ctx.copilot_token,
        &ctx.account_type,
        &vscode_ver,
        &payload,
    )
    .await
    .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(result))
}

// ── Models ──────────────────────────────────────────────────────────────────

pub async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    let models = state.models.read().await;
    match &*models {
        Some(m) => {
            let data: Vec<serde_json::Value> = m
                .data
                .iter()
                .map(|model| {
                    serde_json::json!({
                        "id": model.id,
                        "object": "model",
                        "created": 0,
                        "owned_by": model.vendor,
                        "display_name": model.name,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "object": "list",
                "data": data
            }))
        }
        None => Json(serde_json::json!({
            "object": "list",
            "data": []
        })),
    }
}

// ── Token (debug) ───────────────────────────────────────────────────────────

pub async fn handle_token(State(state): State<AppState>) -> impl IntoResponse {
    let accounts = state.store.get_active_accounts().await;
    let token = accounts
        .first()
        .and_then(|a| a.copilot_token.clone())
        .unwrap_or_default();
    Json(serde_json::json!({ "token": token }))
}

// ── Usage ───────────────────────────────────────────────────────────────────

pub async fn handle_usage(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let accounts = state.store.get_active_accounts().await;
    let account = accounts
        .first()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "No active accounts"))?;

    let usage = crate::services::github::get_copilot_usage(
        &state.http_client,
        &account.github_token,
    )
    .await
    .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(usage))
}

// ── Translation helpers ─────────────────────────────────────────────────────

fn translate_anthropic_to_openai(anthropic: &serde_json::Value) -> serde_json::Value {
    let model = anthropic["model"].as_str().unwrap_or("gpt-4o");
    let max_tokens = anthropic["max_tokens"].as_u64().unwrap_or(4096);
    let stream = anthropic["stream"].as_bool().unwrap_or(false);
    let temperature = anthropic.get("temperature");
    let top_p = anthropic.get("top_p");

    // Convert messages
    let mut openai_messages: Vec<serde_json::Value> = Vec::new();

    // System message
    if let Some(system) = anthropic.get("system") {
        let system_text = if let Some(s) = system.as_str() {
            s.to_string()
        } else if let Some(arr) = system.as_array() {
            arr.iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        if !system_text.is_empty() {
            openai_messages.push(serde_json::json!({
                "role": "system",
                "content": system_text
            }));
        }
    }

    // Convert message array
    if let Some(messages) = anthropic["messages"].as_array() {
        for msg in messages {
            let role = msg["role"].as_str().unwrap_or("user");
            let openai_role = match role {
                "assistant" => "assistant",
                _ => "user",
            };

            // Handle content blocks
            if let Some(content_str) = msg["content"].as_str() {
                openai_messages.push(serde_json::json!({
                    "role": openai_role,
                    "content": content_str
                }));
            } else if let Some(blocks) = msg["content"].as_array() {
                let mut parts: Vec<serde_json::Value> = Vec::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();

                for block in blocks {
                    let block_type = block["type"].as_str().unwrap_or("text");
                    match block_type {
                        "text" => {
                            parts.push(serde_json::json!({
                                "type": "text",
                                "text": block["text"].as_str().unwrap_or("")
                            }));
                        }
                        "image" => {
                            if let Some(source) = block.get("source") {
                                let media_type = source["media_type"].as_str().unwrap_or("image/png");
                                let data = source["data"].as_str().unwrap_or("");
                                parts.push(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{media_type};base64,{data}")
                                    }
                                }));
                            }
                        }
                        "tool_use" => {
                            tool_calls.push(serde_json::json!({
                                "id": block["id"],
                                "type": "function",
                                "function": {
                                    "name": block["name"],
                                    "arguments": serde_json::to_string(
                                        block.get("input").unwrap_or(&serde_json::json!({}))
                                    ).unwrap_or_default()
                                }
                            }));
                        }
                        "tool_result" => {
                            let content = if let Some(c) = block["content"].as_str() {
                                c.to_string()
                            } else if let Some(arr) = block["content"].as_array() {
                                arr.iter()
                                    .filter_map(|b| b["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            } else {
                                String::new()
                            };
                            openai_messages.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": block["tool_use_id"],
                                "content": content
                            }));
                            continue;
                        }
                        "thinking" => {
                            // Include thinking as text
                            if let Some(text) = block["thinking"].as_str() {
                                parts.push(serde_json::json!({
                                    "type": "text",
                                    "text": text
                                }));
                            }
                        }
                        _ => {}
                    }
                }

                if !tool_calls.is_empty() {
                    let mut msg_obj = serde_json::json!({
                        "role": "assistant",
                        "tool_calls": tool_calls
                    });
                    if !parts.is_empty() {
                        let text: String = parts.iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            msg_obj["content"] = serde_json::json!(text);
                        }
                    }
                    openai_messages.push(msg_obj);
                } else if parts.len() == 1 && parts[0]["type"] == "text" {
                    openai_messages.push(serde_json::json!({
                        "role": openai_role,
                        "content": parts[0]["text"]
                    }));
                } else if !parts.is_empty() {
                    openai_messages.push(serde_json::json!({
                        "role": openai_role,
                        "content": parts
                    }));
                }
            }
        }
    }

    let mut result = serde_json::json!({
        "model": model,
        "messages": openai_messages,
        "max_tokens": max_tokens,
        "stream": stream,
    });

    if let Some(t) = temperature {
        result["temperature"] = t.clone();
    }
    if let Some(p) = top_p {
        result["top_p"] = p.clone();
    }

    // Convert tools
    if let Some(tools) = anthropic.get("tools") {
        if let Some(tools_arr) = tools.as_array() {
            let openai_tools: Vec<serde_json::Value> = tools_arr
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tool["name"],
                            "description": tool.get("description").unwrap_or(&serde_json::json!("")),
                            "parameters": tool.get("input_schema").unwrap_or(&serde_json::json!({}))
                        }
                    })
                })
                .collect();
            result["tools"] = serde_json::json!(openai_tools);
        }
    }

    // Stream options for streaming
    if stream {
        result["stream_options"] = serde_json::json!({"include_usage": true});
    }

    result
}

fn translate_openai_to_anthropic(
    openai: &serde_json::Value,
    model: &str,
) -> serde_json::Value {
    let mut content: Vec<serde_json::Value> = Vec::new();

    if let Some(choices) = openai["choices"].as_array() {
        for choice in choices {
            let message = &choice["message"];
            if let Some(text) = message["content"].as_str() {
                if !text.is_empty() {
                    content.push(serde_json::json!({
                        "type": "text",
                        "text": text
                    }));
                }
            }
            if let Some(tool_calls) = message["tool_calls"].as_array() {
                for tc in tool_calls {
                    let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                    let args: serde_json::Value =
                        serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                    content.push(serde_json::json!({
                        "type": "tool_use",
                        "id": tc["id"],
                        "name": tc["function"]["name"],
                        "input": args
                    }));
                }
            }
        }
    }

    let stop_reason = openai["choices"][0]["finish_reason"]
        .as_str()
        .map(|r| match r {
            "stop" => "end_turn",
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            _ => "end_turn",
        })
        .unwrap_or("end_turn");

    let usage = &openai["usage"];

    serde_json::json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage["prompt_tokens"].as_u64().unwrap_or(0),
            "output_tokens": usage["completion_tokens"].as_u64().unwrap_or(0),
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    })
}

/// Translate streaming OpenAI SSE chunks → Anthropic SSE events
fn translate_stream_to_anthropic(
    byte_stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    model: &str,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let model = model.to_string();
    let msg_id = format!("msg_{}", uuid::Uuid::new_v4());

    async_stream::stream! {
        let mut content_index: i32 = -1;
        #[allow(unused_assignments)]
        let mut started = false;
        let mut buffer = String::new();

        // Emit message_start
        let start_event = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        yield Ok(Bytes::from(format!("event: message_start\ndata: {}\n\n", start_event)));
        started = true;

        futures::pin_mut!(byte_stream);

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                    break;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                for line in event_block.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data.trim() == "[DONE]" {
                            // Emit message_delta with stop
                            let delta = serde_json::json!({
                                "type": "message_delta",
                                "delta": {"stop_reason": "end_turn"},
                                "usage": {"output_tokens": 0}
                            });
                            yield Ok(Bytes::from(format!("event: message_delta\ndata: {}\n\n", delta)));

                            let stop = serde_json::json!({"type": "message_stop"});
                            yield Ok(Bytes::from(format!("event: message_stop\ndata: {}\n\n", stop)));
                            break;
                        }

                        if let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) {
                            if let Some(choices) = chunk_json["choices"].as_array() {
                                for choice in choices {
                                    let delta = &choice["delta"];

                                    // Text content
                                    if let Some(text) = delta["content"].as_str() {
                                        if content_index < 0 {
                                            content_index = 0;
                                            let block_start = serde_json::json!({
                                                "type": "content_block_start",
                                                "index": content_index,
                                                "content_block": {"type": "text", "text": ""}
                                            });
                                            yield Ok(Bytes::from(format!("event: content_block_start\ndata: {}\n\n", block_start)));
                                        }
                                        let text_delta = serde_json::json!({
                                            "type": "content_block_delta",
                                            "index": content_index,
                                            "delta": {"type": "text_delta", "text": text}
                                        });
                                        yield Ok(Bytes::from(format!("event: content_block_delta\ndata: {}\n\n", text_delta)));
                                    }

                                    // Tool calls
                                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                                        for tc in tool_calls {
                                            let tc_idx = tc["index"].as_i64().unwrap_or(0) as i32;
                                            let new_idx = tc_idx + 1; // offset by 1 (text is index 0)

                                            if let Some(func) = tc.get("function") {
                                                if func.get("name").is_some() {
                                                    let block_start = serde_json::json!({
                                                        "type": "content_block_start",
                                                        "index": new_idx,
                                                        "content_block": {
                                                            "type": "tool_use",
                                                            "id": tc.get("id").cloned().unwrap_or(serde_json::json!("")),
                                                            "name": func["name"],
                                                            "input": {}
                                                        }
                                                    });
                                                    yield Ok(Bytes::from(format!("event: content_block_start\ndata: {}\n\n", block_start)));
                                                }
                                                if let Some(args) = func["arguments"].as_str() {
                                                    if !args.is_empty() {
                                                        let tc_delta = serde_json::json!({
                                                            "type": "content_block_delta",
                                                            "index": new_idx,
                                                            "delta": {
                                                                "type": "input_json_delta",
                                                                "partial_json": args
                                                            }
                                                        });
                                                        yield Ok(Bytes::from(format!("event: content_block_delta\ndata: {}\n\n", tc_delta)));
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Finish reason
                                    if let Some(finish) = choice["finish_reason"].as_str() {
                                        let stop_reason = match finish {
                                            "stop" => "end_turn",
                                            "length" => "max_tokens",
                                            "tool_calls" => "tool_use",
                                            _ => "end_turn",
                                        };

                                        // Close content blocks
                                        if content_index >= 0 {
                                            let block_stop = serde_json::json!({
                                                "type": "content_block_stop",
                                                "index": content_index
                                            });
                                            yield Ok(Bytes::from(format!("event: content_block_stop\ndata: {}\n\n", block_stop)));
                                        }

                                        let msg_delta = serde_json::json!({
                                            "type": "message_delta",
                                            "delta": {"stop_reason": stop_reason},
                                            "usage": {"output_tokens": 0}
                                        });
                                        yield Ok(Bytes::from(format!("event: message_delta\ndata: {}\n\n", msg_delta)));
                                    }
                                }
                            }

                            // Usage info
                            if let Some(usage) = chunk_json.get("usage") {
                                if usage["total_tokens"].as_u64().unwrap_or(0) > 0 {
                                    let delta = serde_json::json!({
                                        "type": "message_delta",
                                        "delta": {},
                                        "usage": {
                                            "output_tokens": usage["completion_tokens"].as_u64().unwrap_or(0)
                                        }
                                    });
                                    yield Ok(Bytes::from(format!("event: message_delta\ndata: {}\n\n", delta)));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Final message_stop if not already sent
        if started {
            let stop = serde_json::json!({"type": "message_stop"});
            yield Ok(Bytes::from(format!("event: message_stop\ndata: {}\n\n", stop)));
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
