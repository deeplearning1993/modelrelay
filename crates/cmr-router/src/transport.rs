use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
};

use axum::extract::ws::{Message, WebSocket};
use axum::{
    Json,
    body::Body,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use chrono::Utc;
use cmr_providers::{
    AuthStyle, ProviderAdapter, ProviderPreset, StreamState, adapter_for_preset, custom_tool_names,
    preset_by_id,
};
use cmr_storage::{
    CompactionRecord, ProviderOwnerId, ResponseRecord, ResponseStatus, RouterConfig, SecretRef,
    compaction_key,
};
use eventsource_stream::Eventsource;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as UpstreamWsMessage, client::IntoClientRequest},
};
use uuid::Uuid;

use crate::{
    Result, RouterError,
    server::{AppState, raw_response, reject_cross_origin},
};

const WARMUP_RESPONSE_ID_PREFIX: &str = "cmr_warmup_";
const COMPACT_RESPONSE_ID_PREFIX: &str = "cmr_compact_";
const PORTABLE_SUMMARY_OPERATION: &str = "portable_compaction_summary_v1";
const PORTABLE_SUMMARY_INSTRUCTIONS: &str = "Create a concise provider-neutral continuation summary. Preserve user requirements, decisions, files, tool calls and tool results. Do not include private chain-of-thought. Return only the summary as plain text.";

type OfficialWebSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamTerminal {
    Completed,
    Incomplete,
    Failed,
}

struct Target {
    provider_id: String,
    public_model: String,
    upstream_model: String,
    preset: ProviderPreset,
    max_output_tokens: Option<u64>,
    external: bool,
    secret_ref: Option<String>,
    provider_owner_id: Option<ProviderOwnerId>,
}

#[derive(Default)]
struct StreamTapState {
    done_items: BTreeMap<u64, Value>,
    done_item_ids: BTreeMap<String, u64>,
    output: Vec<Value>,
    terminal_status: Option<ResponseStatus>,
    incomplete_details: Option<Value>,
}

#[derive(Debug, Eq, PartialEq)]
struct CanonicalFunctionCall {
    call_id: Option<String>,
    id: Option<String>,
    name: String,
    arguments: String,
}

pub(crate) async fn responses(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(mut request): Json<Value>,
) -> Result<Response> {
    reject_cross_origin(&headers)?;
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut target = resolve_target(&state, &request).await?;
    tracing::info!(
        model = %target.public_model,
        provider = %target.provider_id,
        external = target.external,
        "http turn target"
    );
    validate_external_max_output_tokens(&request, &target)?;
    strip_external_context_management(&mut request, target.external);
    validate_context_management(&request)?;
    require_request_account_scope(&state, &headers, &request, target.external, uri.query()).await?;
    target.provider_owner_id = provider_owner_id(&state, &headers, &target)?;
    bind_adapter_owner(&mut target);
    if take_compaction_trigger(&mut request)? {
        let response = perform_compaction(&state, &headers, request, &target).await?;
        return if stream {
            Ok(compaction_sse_response(&response))
        } else {
            Ok(raw_response(
                StatusCode::OK,
                "application/json",
                serde_json::to_vec(&response)?.into(),
            ))
        };
    }
    let turn_input = canonical_current_input_for_target(&state, input_items(&request)?, &target)?;
    let previous = request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let session = session_id(&state, previous.as_deref(), &request);
    prepare_replay_for_target(&state, &mut request, &target)?;
    ensure_external_harness_tools(&state, &mut request, &target)?;
    if stream {
        stream_response(
            state, headers, request, target, turn_input, previous, session,
        )
        .await
    } else {
        nonstream_response(
            &state, &headers, request, target, turn_input, previous, session,
        )
        .await
    }
}

pub(crate) async fn compact(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Response> {
    reject_cross_origin(&headers)?;
    let mut target = resolve_target(&state, &request).await?;
    validate_external_max_output_tokens(&request, &target)?;
    validate_context_management(&request)?;
    require_request_account_scope(&state, &headers, &request, target.external, uri.query()).await?;
    target.provider_owner_id = provider_owner_id(&state, &headers, &target)?;
    bind_adapter_owner(&mut target);
    // Both official and external compaction first create a provider-neutral
    // summary. The authenticated official backend then turns only that summary
    // into a genuine encrypted compaction item. This gives every opaque item a
    // portable mapping without ever fabricating encrypted_content locally.
    let response = perform_compaction(&state, &headers, request, &target).await?;
    Ok(raw_response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&response)?.into(),
    ))
}

async fn perform_compaction(
    state: &AppState,
    headers: &HeaderMap,
    mut request: Value,
    target: &Target,
) -> Result<Value> {
    let raw_turn_input = input_items(&request)?;
    let turn_input = canonical_current_input_for_target(state, raw_turn_input.clone(), target)?;
    let previous = request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    ensure_compaction_tools_closed(state, previous.as_deref(), &raw_turn_input)?;
    let session_id = session_id(state, previous.as_deref(), &request);
    prepare_replay_for_target(state, &mut request, target)?;
    let object = request
        .as_object_mut()
        .ok_or_else(|| RouterError::bad_request("request must be an object"))?;
    object.insert(
        "instructions".into(),
        Value::String(PORTABLE_SUMMARY_INSTRUCTIONS.into()),
    );
    object.insert(
        "metadata".into(),
        json!({"cmr_internal_operation": PORTABLE_SUMMARY_OPERATION}),
    );
    object.insert("stream".into(), Value::Bool(false));
    object.remove("tools");
    object.remove("additional_tools");
    object.remove("tool_choice");
    object.remove("parallel_tool_calls");
    object.remove("generate");

    let summary_response = if target.external {
        execute_external_nonstream(state, &request, target).await?
    } else {
        official_json_value(state, headers, "/responses", request).await?
    };
    let summary = completed_visible_summary(&summary_response, "provider-neutral compaction")?;
    let official_model = state.config.official_compaction_model.clone()
        .or_else(|| (!target.external).then(|| target.public_model.clone()))
        .or_else(|| state.official_models.try_read().ok().and_then(|models| models.first()
            .and_then(crate::catalog::model_id).map(str::to_owned)))
        .ok_or_else(|| RouterError::bad_request(
            "official model catalog is not cached; open the model picker once or set official_compaction_model"))?;
    let compact_body = json!({
        "model": official_model,
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":summary}]}]
    });
    let response = official_json_value(state, headers, "/responses/compact", compact_body).await?;
    let item = validate_compaction_response(&response)?;
    let created_at = Utc::now();
    let compaction = CompactionRecord {
        response_id: compaction_key(item)?,
        source_provider: "official".into(),
        source_owner_id: official_owner_id(state, headers)?,
        portable_summary: summary,
        encrypted_item: item.clone(),
        created_at,
    };
    let normalized = normalize_compaction_response(&response, &target.public_model, item);
    let response_id = normalized
        .get("id")
        .and_then(Value::as_str)
        .expect("normalized compaction response always has an id")
        .to_owned();
    state.sessions.record_response_with_compactions(
        &ResponseRecord {
            id: response_id,
            session_id,
            previous_response_id: previous,
            provider_id: target.provider_id.clone(),
            provider_owner_id: target.provider_owner_id.clone(),
            model_id: target.public_model.clone(),
            input: turn_input,
            output: vec![item.clone()],
            status: ResponseStatus::Completed,
            incomplete_details: None,
            created_at,
        },
        &[compaction],
    )?;
    Ok(normalized)
}

async fn nonstream_response(
    state: &AppState,
    headers: &HeaderMap,
    request: Value,
    target: Target,
    input: Vec<Value>,
    previous: Option<String>,
    session_id: String,
) -> Result<Response> {
    let response = if target.external {
        execute_external_nonstream(state, &request, &target).await?
    } else {
        official_json_value(state, headers, "/responses", request.clone()).await?
    };
    let (id, output, status, incomplete_details) = validate_nonstream_terminal(&response)?;
    let record = ResponseRecord {
        id,
        session_id,
        previous_response_id: previous,
        provider_id: target.provider_id,
        provider_owner_id: target.provider_owner_id,
        model_id: target.public_model,
        input,
        output,
        status,
        incomplete_details,
        created_at: Utc::now(),
    };
    finalize_response_record(state, headers, &record).await?;
    Ok(raw_response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&response)?.into(),
    ))
}

async fn execute_external_nonstream(
    state: &AppState,
    request: &Value,
    target: &Target,
) -> Result<Value> {
    let adapter = adapter_for_preset(target.preset.clone());
    let mut converted = adapter.encode_request(request)?;
    converted
        .as_object_mut()
        .ok_or_else(|| RouterError::bad_request("converted request is not an object"))?
        .insert("model".into(), Value::String(target.upstream_model.clone()));
    let url = endpoint(target, adapter.as_ref(), false);
    let upstream = external_request(state, target, state.client.post(url), &converted)?
        .send()
        .await?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(RouterError::upstream(
            status,
            "external provider rejected the request",
        ));
    }
    let value: Value = upstream.json().await?;
    let id = format!("resp_{}", Uuid::new_v4());
    let response = adapter.decode_response(&value, &id)?;
    expose_public_model(response, &target.public_model)
}

fn expose_public_model(mut response: Value, public_model: &str) -> Result<Value> {
    response
        .as_object_mut()
        .ok_or_else(|| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "external provider adapter returned a non-object response",
            )
        })?
        .insert("model".into(), Value::String(public_model.to_owned()));
    Ok(response)
}

// Streaming is a single ordered protocol state machine; keeping it contiguous
// makes terminal-event and persistence ordering reviewable.
#[allow(clippy::too_many_lines)]
async fn stream_response(
    state: AppState,
    headers: HeaderMap,
    mut request: Value,
    target: Target,
    input: Vec<Value>,
    previous: Option<String>,
    session: String,
) -> Result<Response> {
    request
        .as_object_mut()
        .ok_or_else(|| RouterError::bad_request("request must be an object"))?
        .insert("stream".into(), Value::Bool(true));
    let response_id = format!("resp_{}", Uuid::new_v4());
    let adapter = adapter_for_preset(target.preset.clone());
    let upstream = if target.external {
        let mut converted = adapter.encode_request(&request)?;
        converted
            .as_object_mut()
            .ok_or_else(|| RouterError::bad_request("converted request is not an object"))?
            .insert("model".into(), Value::String(target.upstream_model.clone()));
        let url = endpoint(&target, adapter.as_ref(), true);
        external_request(&state, &target, state.client.post(url), &converted)?
            .send()
            .await?
    } else {
        let url = format!(
            "{}/responses",
            state.config.official_base_url.trim_end_matches('/')
        );
        state
            .client
            .post(url)
            .headers(forward_headers(&headers, true, false))
            .json(&request)
            .send()
            .await?
    };
    let status = upstream.status();
    if !status.is_success() {
        let (code, message) = if target.external {
            (
                "external_provider_error",
                "external provider rejected the request",
            )
        } else {
            (
                "official_upstream_error",
                "official ChatGPT backend rejected the request",
            )
        };
        let body = serde_json::to_vec(&json!({
            "error": {"code": code, "message": message}
        }))?;
        return Ok(raw_response(status, "application/json", body.into()));
    }

    let (sender, receiver) = mpsc::channel::<std::result::Result<Bytes, Infallible>>(32);
    let custom_tool_names = custom_tool_names(&request);
    tokio::spawn(async move {
        let mut stream_state = StreamState::new(&response_id, &target.public_model);
        stream_state.set_custom_tools(custom_tool_names);
        let mut tap_state = StreamTapState::default();
        let mut actual_id = response_id.clone();
        let mut next_error_sequence = 0_u64;
        let created_at = Utc::now();
        let mut begin_record: Option<ResponseRecord> = None;
        // A persistable terminal event is a commit acknowledgement: do not let the
        // client observe it until the canonical turn has been durably recorded.
        // Each output_item.done is journaled before delivery, so a crash cannot
        // erase an already executable function call from the local ledger.
        let mut terminal_event: Option<Value> = None;
        let mut events = upstream.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    let value = response_error_event(
                        "upstream_stream_error",
                        &error.to_string(),
                        next_error_sequence,
                    );
                    let _ = send_sse(&sender, &value).await;
                    return;
                }
            };
            let chunk = match parse_sse_data(&event.data) {
                Ok(Some(value)) => value,
                Ok(None) => continue,
                Err(error) => {
                    let value =
                        response_error_event(error.code, &error.message, next_error_sequence);
                    let _ = send_sse(&sender, &value).await;
                    return;
                }
            };
            if terminal_event.is_some() {
                let value = response_error_event(
                    "upstream_stream_protocol_error",
                    "upstream emitted a data event after a terminal Responses event",
                    next_error_sequence,
                );
                let _ = send_sse(&sender, &value).await;
                return;
            }
            let converted = if target.external {
                match adapter.decode_stream_chunk(&mut stream_state, &chunk) {
                    Ok(events) => events,
                    Err(error) => {
                        let value = response_error_event(
                            "adapter_error",
                            &error.to_string(),
                            next_error_sequence,
                        );
                        let _ = send_sse(&sender, &value).await;
                        return;
                    }
                }
            } else {
                vec![chunk]
            };
            for value in converted {
                if let Some(sequence) = value.get("sequence_number").and_then(Value::as_u64) {
                    next_error_sequence = next_error_sequence.max(sequence.saturating_add(1));
                }
                if let Err(error) = tap_event(&value, &mut actual_id, &mut tap_state) {
                    let value =
                        response_error_event(error.code, &error.message, next_error_sequence);
                    let _ = send_sse(&sender, &value).await;
                    return;
                }
                if let Err(error) = persist_stream_event_before_delivery(
                    &state,
                    &value,
                    &actual_id,
                    &mut begin_record,
                    &target,
                    &input,
                    previous.as_deref(),
                    &session,
                    created_at,
                ) {
                    let value =
                        response_error_event(error.code, &error.message, next_error_sequence);
                    let _ = send_sse(&sender, &value).await;
                    return;
                }
                match stream_terminal(&value) {
                    Some(StreamTerminal::Completed | StreamTerminal::Incomplete) => {
                        terminal_event = Some(value);
                        continue;
                    }
                    Some(StreamTerminal::Failed) => {
                        let _ = send_sse(&sender, &value).await;
                        return;
                    }
                    None => {}
                }
                if send_sse(&sender, &value).await.is_err() {
                    return;
                }
            }
            if stream_batch_completed(&tap_state) {
                // `response.completed` is the protocol terminator. Commit as soon
                // as its entire converted batch has been validated instead of
                // waiting for an upstream [DONE], EOF, or keepalive timeout.
                break;
            }
        }
        if target.external && !stream_batch_completed(&tap_state) {
            match adapter.finish_stream(&mut stream_state) {
                Ok(final_events) => {
                    for value in final_events {
                        if let Some(sequence) = value.get("sequence_number").and_then(Value::as_u64)
                        {
                            next_error_sequence =
                                next_error_sequence.max(sequence.saturating_add(1));
                        }
                        if let Err(error) = tap_event(&value, &mut actual_id, &mut tap_state) {
                            let value = response_error_event(
                                error.code,
                                &error.message,
                                next_error_sequence,
                            );
                            let _ = send_sse(&sender, &value).await;
                            return;
                        }
                        if let Err(error) = persist_stream_event_before_delivery(
                            &state,
                            &value,
                            &actual_id,
                            &mut begin_record,
                            &target,
                            &input,
                            previous.as_deref(),
                            &session,
                            created_at,
                        ) {
                            let value = response_error_event(
                                error.code,
                                &error.message,
                                next_error_sequence,
                            );
                            let _ = send_sse(&sender, &value).await;
                            return;
                        }
                        match stream_terminal(&value) {
                            Some(StreamTerminal::Completed | StreamTerminal::Incomplete) => {
                                terminal_event = Some(value);
                                continue;
                            }
                            Some(StreamTerminal::Failed) => {
                                let _ = send_sse(&sender, &value).await;
                                return;
                            }
                            None => {}
                        }
                        if send_sse(&sender, &value).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let value = response_error_event(
                        "adapter_error",
                        &error.to_string(),
                        next_error_sequence,
                    );
                    let _ = send_sse(&sender, &value).await;
                    return;
                }
            }
        }
        let Some(terminal_event) = terminal_event else {
            let value = response_error_event(
                "upstream_stream_incomplete",
                "upstream stream ended without response.completed or response.incomplete",
                next_error_sequence,
            );
            let _ = send_sse(&sender, &value).await;
            return;
        };
        let Some(mut record) = begin_record else {
            let value = response_error_event(
                "upstream_stream_protocol_error",
                "upstream terminal event arrived without response.created",
                next_error_sequence,
            );
            let _ = send_sse(&sender, &value).await;
            return;
        };
        record.status = tap_state
            .terminal_status
            .expect("terminal event sets a terminal status");
        record.output = tap_state.output;
        record.incomplete_details = tap_state.incomplete_details;
        if let Err(error) = finalize_response_record(&state, &headers, &record).await {
            let value = response_error_event(
                "router_persistence_error",
                &format!("failed to persist response context: {error}"),
                next_error_sequence,
            );
            let _ = send_sse(&sender, &value).await;
            return;
        }
        let _ = send_sse(&sender, &terminal_event).await;
    });
    let body_stream = ReceiverStream::new(receiver);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .expect("static stream headers are valid"))
}

async fn finalize_response_record(
    state: &AppState,
    headers: &HeaderMap,
    record: &ResponseRecord,
) -> Result<()> {
    let compaction_items = record
        .output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        .cloned()
        .collect::<Vec<_>>();
    if compaction_items.is_empty() {
        state.sessions.record_response(record)?;
        return Ok(());
    }
    if compaction_items.len() != 1 || record.provider_id != "official" {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            "only one genuine official compaction output can be persisted",
        ));
    }
    let mut summary_input = if let Some(previous) = record.previous_response_id.as_deref() {
        state.sessions.replay_items(previous, "portable-summary")?
    } else {
        Vec::new()
    };
    summary_input.extend(record.input.iter().cloned());
    let summary_model = state
        .config
        .official_compaction_model
        .clone()
        .unwrap_or_else(|| record.model_id.clone());
    let summary_response = official_json_value(
        state,
        headers,
        "/responses",
        json!({
            "model": summary_model,
            "instructions": PORTABLE_SUMMARY_INSTRUCTIONS,
            "input": summary_input,
            "stream": false,
            "metadata": {"cmr_internal_operation": PORTABLE_SUMMARY_OPERATION}
        }),
    )
    .await?;
    let summary = completed_visible_summary(&summary_response, "automatic compaction")?;
    let item = compaction_items
        .into_iter()
        .next()
        .expect("checked exactly one compaction item");
    let mapping = CompactionRecord {
        response_id: compaction_key(&item)?,
        source_provider: "official".into(),
        source_owner_id: official_owner_id(state, headers)?,
        portable_summary: summary,
        encrypted_item: item,
        created_at: record.created_at,
    };
    state
        .sessions
        .record_response_with_compactions(record, &[mapping])?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn persist_stream_event_before_delivery(
    state: &AppState,
    event: &Value,
    response_id: &str,
    begin_record: &mut Option<ResponseRecord>,
    target: &Target,
    input: &[Value],
    previous: Option<&str>,
    session_id: &str,
    created_at: chrono::DateTime<Utc>,
) -> Result<()> {
    match event.get("type").and_then(Value::as_str) {
        Some("response.created") => {
            if begin_record.is_some() {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "upstream repeated response.created",
                ));
            }
            if response_id.trim().is_empty()
                || event.pointer("/response/id").and_then(Value::as_str) != Some(response_id)
                || event.pointer("/response/status").and_then(Value::as_str) != Some("in_progress")
                || !event
                    .pointer("/response/output")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "response.created must contain a non-empty id, in_progress status, and empty output",
                ));
            }
            let record = ResponseRecord {
                id: response_id.to_owned(),
                session_id: session_id.to_owned(),
                previous_response_id: previous.map(str::to_owned),
                provider_id: target.provider_id.clone(),
                provider_owner_id: target.provider_owner_id.clone(),
                model_id: target.public_model.clone(),
                input: input.to_vec(),
                output: Vec::new(),
                status: ResponseStatus::InProgress,
                incomplete_details: None,
                created_at,
            };
            state.sessions.begin_response(&record)?;
            *begin_record = Some(record);
        }
        Some("response.output_item.done") => {
            let record = begin_record.as_ref().ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "response.output_item.done arrived before response.created",
                )
            })?;
            if event
                .get("response_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id != record.id)
            {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "response.output_item.done response_id changed within the stream",
                ));
            }
            let output_index = event
                .get("output_index")
                .and_then(Value::as_u64)
                .and_then(|index| u32::try_from(index).ok())
                .ok_or_else(|| {
                    RouterError::upstream(
                        StatusCode::BAD_GATEWAY,
                        "response.output_item.done has an invalid output_index",
                    )
                })?;
            let item = event.get("item").ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "response.output_item.done has no item",
                )
            })?;
            state
                .sessions
                .journal_output_item(&record.id, output_index, item)?;
        }
        Some("response.completed" | "response.incomplete") => {
            let record = begin_record.as_ref().ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "terminal Responses event arrived before response.created",
                )
            })?;
            if response_id != record.id
                || event.pointer("/response/id").and_then(Value::as_str) != Some(record.id.as_str())
            {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "terminal response id changed within the stream",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

// The WebSocket session is intentionally one ordered state machine so active
// model inheritance and error-frame ordering remain explicit.
#[allow(clippy::too_many_lines)]
pub(crate) async fn websocket_loop(
    state: AppState,
    headers: HeaderMap,
    upgrade_query: Option<String>,
    mut socket: WebSocket,
) {
    let mut active_model: Option<String> = None;
    let mut official_socket: Option<OfficialWebSocket> = None;
    while let Some(Ok(message)) = socket.next().await {
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
                continue;
            }
            _ => continue,
        };
        let mut envelope: Value = if let Ok(value) = serde_json::from_str(&text) {
            value
        } else {
            let _ = socket
                .send(Message::Text(
                    response_error_event("invalid_json", "invalid JSON frame", 0)
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        };
        if envelope.get("type").and_then(Value::as_str) != Some("response.create") {
            let _ = socket
                .send(Message::Text(
                    response_error_event("invalid_event", "expected response.create", 0)
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }
        {
            let Some(object) = envelope.as_object_mut() else {
                continue;
            };
            object.remove("type");
            if !object.contains_key("model") {
                if let Some(model) = &active_model {
                    object.insert("model".into(), Value::String(model.clone()));
                }
            }
        }
        let mut target = match resolve_target(&state, &envelope).await {
            Ok(target) => target,
            Err(error) => {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(error.code, &error.message, 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
        };
        tracing::info!(
            model = %target.public_model,
            provider = %target.provider_id,
            external = target.external,
            "ws turn target"
        );
        if let Err(error) = validate_external_max_output_tokens(&envelope, &target) {
            let _ = socket
                .send(Message::Text(
                    response_error_event(error.code, &error.message, 0)
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }
        strip_external_context_management(&mut envelope, target.external);
        if let Err(error) = validate_context_management(&envelope) {
            let _ = socket
                .send(Message::Text(
                    response_error_event(error.code, &error.message, 0)
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }
        if let Err(error) = require_request_account_scope(
            &state,
            &headers,
            &envelope,
            target.external,
            upgrade_query.as_deref(),
        )
        .await
        {
            let _ = socket
                .send(Message::Text(
                    response_error_event(error.code, &error.message, 0)
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }
        target.provider_owner_id = match provider_owner_id(&state, &headers, &target) {
            Ok(owner) => owner,
            Err(error) => {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(error.code, &error.message, 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
        };
        bind_adapter_owner(&mut target);
        let accepted_model = target.public_model.clone();
        let is_compaction = match take_compaction_trigger(&mut envelope) {
            Ok(value) => value,
            Err(error) => {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(error.code, &error.message, 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
        };
        let input = match input_items(&envelope) {
            Ok(input) => input,
            Err(error) => {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(error.code, &error.message, 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
        };
        let input = match canonical_current_input_for_target(&state, input, &target) {
            Ok(input) => input,
            Err(error) => {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(error.code, &error.message, 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
        };
        let previous = envelope
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let session = session_id(&state, previous.as_deref(), &envelope);
        if is_compaction {
            match perform_compaction(&state, &headers, envelope, &target).await {
                Ok(response) => {
                    active_model = Some(accepted_model.clone());
                    for event in compaction_events(&response) {
                        if socket
                            .send(Message::Text(event.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = socket
                        .send(Message::Text(
                            response_error_event(error.code, &error.message, 0)
                                .to_string()
                                .into(),
                        ))
                        .await;
                }
            }
            continue;
        }
        if should_relay_official_websocket(&target, &envelope) {
            if let Err(error) = prepare_replay_for_target(&state, &mut envelope, &target) {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(error.code, &error.message, 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
            match relay_official_websocket_turn(
                &state,
                &headers,
                &mut socket,
                &mut official_socket,
                envelope,
                &target,
                input,
                previous,
                session,
            )
            .await
            {
                Ok(true) => active_model = Some(accepted_model),
                Ok(false) => {}
                Err(error) => {
                    official_socket = None;
                    let _ = socket
                        .send(Message::Text(
                            response_error_event(error.code, &error.message, 0)
                                .to_string()
                                .into(),
                        ))
                        .await;
                }
            }
            continue;
        }
        if envelope.get("generate").and_then(Value::as_bool) == Some(false) {
            if target.external
                && previous
                    .as_deref()
                    .is_some_and(|id| match target.provider_owner_id.as_ref() {
                        Some(owner) => state
                            .sessions
                            .replay_items_for_owner(id, &target.provider_id, owner)
                            .is_err(),
                        None => state
                            .sessions
                            .replay_items(id, &target.provider_id)
                            .is_err(),
                    })
            {
                let _ = socket.send(Message::Text(
                    response_error_event(
                        "previous_response_not_found",
                        "external-model WebSocket warmup can only chain from locally recorded history",
                        0,
                    )
                    .to_string()
                    .into(),
                )).await;
                continue;
            }
            // `generate:false` is a local Responses-WebSocket warmup.  Its id
            // has never existed at an upstream provider, so mark it explicitly
            // and force the next turn to replay its canonical input.
            let id = format!("{WARMUP_RESPONSE_ID_PREFIX}{}", Uuid::new_v4());
            let created_at = Utc::now();
            if let Err(error) = state.sessions.record_response(&ResponseRecord {
                id: id.clone(),
                session_id: session,
                previous_response_id: previous,
                provider_id: target.provider_id.clone(),
                provider_owner_id: target.provider_owner_id.clone(),
                model_id: target.public_model.clone(),
                input,
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at,
            }) {
                let _ = socket
                    .send(Message::Text(
                        response_error_event("router_error", &error.to_string(), 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
            let base = json!({
                "id": id,
                "object": "response",
                "created_at": created_at.timestamp(),
                "model": target.public_model,
                "output": []
            });
            let mut in_progress = base.clone();
            in_progress["status"] = Value::String("in_progress".into());
            let mut completed = base;
            completed["status"] = Value::String("completed".into());
            for event in [
                json!({"type":"response.created","sequence_number":0,"response":in_progress}),
                json!({"type":"response.completed","sequence_number":1,"response":completed}),
            ] {
                if socket
                    .send(Message::Text(event.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            active_model = Some(accepted_model);
            continue;
        }
        envelope
            .as_object_mut()
            .expect("validated response.create object")
            .insert("stream".into(), Value::Bool(true));
        if let Err(error) = prepare_replay_for_target(&state, &mut envelope, &target) {
            let _ = socket
                .send(Message::Text(
                    response_error_event(error.code, &error.message, 0)
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }
        if let Err(error) = ensure_external_harness_tools(&state, &mut envelope, &target) {
            let _ = socket
                .send(Message::Text(
                    response_error_event(error.code, &error.message, 0)
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }
        // Reuse the same upstream streaming machinery and decode its local SSE body.
        let response = match stream_response(
            state.clone(),
            headers.clone(),
            envelope,
            target,
            input,
            previous,
            session,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(error.code, &error.message, 0)
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let _ = socket
                .send(Message::Text(
                    response_error_event(
                        "upstream_error",
                        &format!("upstream returned HTTP {status}"),
                        0,
                    )
                    .to_string()
                    .into(),
                ))
                .await;
            continue;
        }
        let mut bytes = response.into_body().into_data_stream();
        let mut buffer = Vec::new();
        let mut accepted_terminal = false;
        let mut stream_failed = false;
        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else {
                let _ = socket
                    .send(Message::Text(
                        response_error_event(
                            "router_stream_error",
                            "router response stream ended with an error",
                            0,
                        )
                        .to_string()
                        .into(),
                    ))
                    .await;
                stream_failed = true;
                break;
            };
            buffer.extend_from_slice(&chunk);
            while let Some(frame) = take_sse_frame(&mut buffer) {
                let Ok(event) = String::from_utf8(frame) else {
                    let _ = socket
                        .send(Message::Text(
                            response_error_event(
                                "invalid_sse_utf8",
                                "router produced invalid SSE UTF-8",
                                0,
                            )
                            .to_string()
                            .into(),
                        ))
                        .await;
                    return;
                };
                let data = event
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data: ")
                            .or_else(|| line.strip_prefix("data:"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !data.is_empty() {
                    let terminal = serde_json::from_str::<Value>(&data)
                        .ok()
                        .and_then(|event| stream_terminal(&event));
                    if socket.send(Message::Text(data.into())).await.is_err() {
                        return;
                    }
                    if matches!(
                        terminal,
                        Some(StreamTerminal::Completed | StreamTerminal::Incomplete)
                    ) {
                        accepted_terminal = true;
                    }
                }
            }
        }
        if accepted_terminal && !stream_failed {
            active_model = Some(accepted_model);
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn relay_official_websocket_turn(
    state: &AppState,
    headers: &HeaderMap,
    client: &mut WebSocket,
    upstream: &mut Option<OfficialWebSocket>,
    mut request: Value,
    target: &Target,
    input: Vec<Value>,
    previous: Option<String>,
    session_id: String,
) -> Result<bool> {
    if upstream.is_none() {
        *upstream = Some(connect_official_websocket(state, headers).await?);
    }
    let object = request
        .as_object_mut()
        .ok_or_else(|| RouterError::bad_request("response.create must be an object"))?;
    object.insert("type".into(), Value::String("response.create".into()));
    object.remove("stream");
    object.remove("background");
    upstream
        .as_mut()
        .expect("official WebSocket was connected")
        .send(UpstreamWsMessage::Text(request.to_string().into()))
        .await
        .map_err(|error| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                format!("official WebSocket send failed: {error}"),
            )
        })?;

    let created_at = Utc::now();
    let mut response_id = String::new();
    let mut tap_state = StreamTapState::default();
    let mut begin_record: Option<ResponseRecord> = None;
    loop {
        let message = upstream
            .as_mut()
            .expect("official WebSocket remains connected")
            .next()
            .await
            .ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "official WebSocket closed before a terminal Responses event",
                )
            })?
            .map_err(|error| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    format!("official WebSocket receive failed: {error}"),
                )
            })?;
        let text = match message {
            UpstreamWsMessage::Text(text) => text,
            UpstreamWsMessage::Ping(payload) => {
                upstream
                    .as_mut()
                    .expect("official WebSocket remains connected")
                    .send(UpstreamWsMessage::Pong(payload))
                    .await
                    .map_err(|error| {
                        RouterError::upstream(
                            StatusCode::BAD_GATEWAY,
                            format!("official WebSocket pong failed: {error}"),
                        )
                    })?;
                continue;
            }
            UpstreamWsMessage::Pong(_) => continue,
            UpstreamWsMessage::Close(_) => {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "official WebSocket closed before a terminal Responses event",
                ));
            }
            UpstreamWsMessage::Binary(_) | UpstreamWsMessage::Frame(_) => {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "official WebSocket emitted a non-text Responses frame",
                ));
            }
        };
        let event: Value = serde_json::from_str(text.as_str()).map_err(|_| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "official WebSocket emitted malformed JSON",
            )
        })?;
        tap_event(&event, &mut response_id, &mut tap_state)?;
        persist_stream_event_before_delivery(
            state,
            &event,
            &response_id,
            &mut begin_record,
            target,
            &input,
            previous.as_deref(),
            &session_id,
            created_at,
        )?;
        match stream_terminal(&event) {
            Some(StreamTerminal::Completed | StreamTerminal::Incomplete) => {
                let mut record = begin_record.ok_or_else(|| {
                    RouterError::upstream(
                        StatusCode::BAD_GATEWAY,
                        "official WebSocket terminal arrived without response.created",
                    )
                })?;
                record.status = tap_state
                    .terminal_status
                    .expect("terminal event supplies status");
                record.output = tap_state.output;
                record.incomplete_details = tap_state.incomplete_details;
                finalize_response_record(state, headers, &record).await?;
                client
                    .send(Message::Text(text.to_string().into()))
                    .await
                    .map_err(|_| RouterError::internal("client WebSocket closed"))?;
                return Ok(true);
            }
            Some(StreamTerminal::Failed) => {
                client
                    .send(Message::Text(text.to_string().into()))
                    .await
                    .map_err(|_| RouterError::internal("client WebSocket closed"))?;
                return Ok(false);
            }
            None => {
                client
                    .send(Message::Text(text.to_string().into()))
                    .await
                    .map_err(|_| RouterError::internal("client WebSocket closed"))?;
            }
        }
    }
}

async fn connect_official_websocket(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<OfficialWebSocket> {
    let mut url = url::Url::parse(state.config.official_base_url.trim_end_matches('/'))
        .map_err(|_| RouterError::internal("official base URL is invalid"))?;
    let websocket_scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => {
            return Err(RouterError::internal(
                "official base URL has no WebSocket scheme mapping",
            ));
        }
    };
    url.set_scheme(websocket_scheme)
        .map_err(|()| RouterError::internal("cannot construct official WebSocket URL"))?;
    let path = format!("{}/responses", url.path().trim_end_matches('/'));
    url.set_path(&path);
    let mut request = url.as_str().into_client_request().map_err(|error| {
        RouterError::internal(format!(
            "cannot construct official WebSocket request: {error}"
        ))
    })?;
    let forwarded = forward_headers(headers, true, false);
    for (name, value) in &forwarded {
        if matches!(name.as_str(), "accept" | "content-type") {
            continue;
        }
        request.headers_mut().append(name, value.clone());
    }
    let (socket, _) = connect_async(request).await.map_err(|error| {
        RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            format!("official WebSocket connection failed: {error}"),
        )
    })?;
    Ok(socket)
}

fn take_sse_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    let (index, delimiter) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(found), None) | (None, Some(found)) => found,
        (None, None) => return None,
    };
    let frame = buffer[..index].to_vec();
    buffer.drain(..index + delimiter);
    Some(frame)
}

async fn resolve_target(state: &AppState, request: &Value) -> Result<Target> {
    let public_model = request
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::bad_request("model is required"))?;
    let model = state.config.models.iter().find(|model| {
        model.id == public_model
            && crate::catalog::is_published_external_model(&state.config, model)
    });
    let Some(model) = model else {
        let official_models = state.official_models.read().await;
        if official_models.is_empty() {
            return Err(RouterError::bad_request(
                "official model catalog is not cached; open the model picker first",
            ));
        }
        if !official_models
            .iter()
            .any(|model| crate::catalog::model_id(model) == Some(public_model))
        {
            return Err(RouterError::bad_request(format!(
                "unknown model: {public_model}"
            )));
        }
        let preset = preset_by_id("openai").expect("built-in OpenAI preset");
        return Ok(Target {
            provider_id: "official".into(),
            public_model: public_model.into(),
            upstream_model: public_model.into(),
            preset,
            max_output_tokens: None,
            external: false,
            secret_ref: None,
            provider_owner_id: None,
        });
    };
    let provider = state
        .config
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.id == model.provider)
        .ok_or_else(|| {
            RouterError::bad_request(format!(
                "provider {} is disabled or missing",
                model.provider
            ))
        })?;
    let mut preset = preset_by_id(&provider.preset)
        .or_else(|| {
            provider.base_url.as_ref().and_then(|url| {
                ProviderPreset::custom_compatible(
                    provider.id.clone(),
                    provider.id.clone(),
                    url.clone(),
                    provider.allow_insecure_http,
                )
                .ok()
            })
        })
        .ok_or_else(|| {
            RouterError::bad_request(format!("unknown provider preset: {}", provider.preset))
        })?;
    if let Some(url) = &provider.base_url {
        validate_endpoint(url, provider.allow_insecure_http)?;
        url.trim_end_matches('/').clone_into(&mut preset.base_url);
    }
    // Provenance belongs to this configured provider instance, not to the
    // reusable preset kind. This prevents reasoning/private payloads from one
    // account using a preset from being replayed to another account using the
    // same preset.
    preset.id.clone_from(&provider.id);
    let max_output_tokens = model
        .max_output_tokens
        .or(preset.capabilities.max_output_tokens);
    Ok(Target {
        provider_id: provider.id.clone(),
        public_model: model.id.clone(),
        upstream_model: model.upstream_model.clone(),
        preset,
        max_output_tokens,
        external: true,
        secret_ref: provider.secret_ref.clone(),
        provider_owner_id: None,
    })
}

fn is_websocket_warmup(request: &Value) -> bool {
    request.get("generate").and_then(Value::as_bool) == Some(false)
}

fn should_relay_official_websocket(target: &Target, request: &Value) -> bool {
    !target.external && !is_websocket_warmup(request)
}

fn provider_owner_id(
    state: &AppState,
    headers: &HeaderMap,
    target: &Target,
) -> Result<Option<ProviderOwnerId>> {
    let Some(instance_id) = state.config_instance_id.as_ref() else {
        return Ok(None);
    };
    if target.external {
        let secret_ref = target
            .secret_ref
            .as_deref()
            .map(SecretRef::parse)
            .transpose()?;
        if secret_ref
            .as_ref()
            .is_some_and(|reference| reference.generation().is_none())
        {
            // A mutable legacy provider/profile reference cannot prove exact
            // credential ownership. It remains usable, but private payloads are
            // deliberately excluded from replay until the credential is rotated
            // into a generation-specific reference.
            return Ok(None);
        }
        return ProviderOwnerId::new(
            instance_id,
            &target.provider_id,
            &target.preset.base_url,
            secret_ref.as_ref(),
        )
        .map(Some)
        .map_err(Into::into);
    }
    official_owner_id(state, headers)
}

fn official_owner_id(state: &AppState, headers: &HeaderMap) -> Result<Option<ProviderOwnerId>> {
    let Some(instance_id) = state.config_instance_id.as_ref() else {
        return Ok(None);
    };
    let Some(generation) = crate::server::chatgpt_account_generation(headers) else {
        return Ok(None);
    };
    ProviderOwnerId::for_credential_generation(
        instance_id,
        "official",
        state.config.official_base_url.trim_end_matches('/'),
        &generation,
    )
    .map(Some)
    .map_err(Into::into)
}

fn bind_adapter_owner(target: &mut Target) {
    if let Some(owner) = &target.provider_owner_id {
        owner.as_str().clone_into(&mut target.preset.id);
    }
}

fn validate_external_max_output_tokens(request: &Value, target: &Target) -> Result<()> {
    if !target.external {
        return Ok(());
    }
    let Some(value) = request.get("max_output_tokens") else {
        return Ok(());
    };
    let requested = value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
        RouterError::bad_request("max_output_tokens must be a positive JSON integer")
    })?;
    if target
        .max_output_tokens
        .is_some_and(|limit| requested > limit)
    {
        return Err(RouterError::bad_request(format!(
            "max_output_tokens {requested} exceeds the effective model limit {}",
            target.max_output_tokens.expect("checked effective limit")
        )));
    }
    Ok(())
}

fn validate_context_management(request: &Value) -> Result<()> {
    let Some(value) = request.get("context_management") else {
        return Ok(());
    };
    let entries = value
        .as_array()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| {
            RouterError::bad_request("context_management must be a non-empty JSON array")
        })?;
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| {
            RouterError::bad_request("each context_management entry must be an object")
        })?;
        if object.get("type").and_then(Value::as_str) != Some("compaction") {
            return Err(RouterError::bad_request(
                "only context_management entries with type=compaction are supported",
            ));
        }
        if let Some(threshold) = object.get("compact_threshold") {
            if threshold.as_u64().is_none_or(|threshold| threshold == 0) {
                return Err(RouterError::bad_request(
                    "context_management.compact_threshold must be a positive JSON integer",
                ));
            }
        }
    }
    Ok(())
}

/// External models have no server-side context management, but a long client
/// conversation legitimately asks for it. Dropping the field lets the turn
/// proceed with the router's full portable replay instead of failing, while
/// official turns keep relaying the field to the real backend.
fn strip_external_context_management(request: &mut Value, external: bool) {
    if !external {
        return;
    }
    if let Some(object) = request.as_object_mut() {
        object.remove("context_management");
    }
}

fn validate_nonstream_terminal(
    response: &Value,
) -> Result<(String, Vec<Value>, ResponseStatus, Option<Value>)> {
    let object = response.as_object().ok_or_else(|| {
        RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            "upstream Responses payload is not an object",
        )
    })?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "upstream Responses payload has no non-empty id",
            )
        })?
        .to_owned();
    let output = object
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "upstream Responses payload has no output array",
            )
        })?;
    canonical_function_calls(&output)?;
    let status = match object.get("status").and_then(Value::as_str) {
        Some("completed") => ResponseStatus::Completed,
        Some("incomplete") => ResponseStatus::Incomplete,
        _ => {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "upstream Responses payload has no persistable terminal status",
            ));
        }
    };
    let incomplete_details = match object.get("incomplete_details") {
        None | Some(Value::Null) => None,
        Some(details @ Value::Object(_)) => Some(details.clone()),
        Some(_) => {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "upstream Responses incomplete_details must be an object or null",
            ));
        }
    };
    if status == ResponseStatus::Completed && incomplete_details.is_some() {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            "completed upstream Responses payload cannot have incomplete_details",
        ));
    }
    Ok((id, output, status, incomplete_details))
}

fn validate_endpoint(value: &str, allow_insecure_http: bool) -> Result<()> {
    let url = url::Url::parse(value)
        .map_err(|_| RouterError::bad_request("invalid provider base URL"))?;
    let loopback = url.host().is_some_and(|host| match host {
        url::Host::Domain(domain) => domain == "localhost",
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && (loopback || allow_insecure_http)) {
        return Err(RouterError::bad_request(
            "provider URL must be HTTPS, except loopback HTTP or an explicit allow_insecure_http opt-in",
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(RouterError::bad_request(
            "provider URL cannot contain credentials, a query string, or a fragment",
        ));
    }
    Ok(())
}

async fn require_request_account_scope(
    state: &AppState,
    headers: &HeaderMap,
    request: &Value,
    external: bool,
    request_query: Option<&str>,
) -> Result<()> {
    let replays_local_history = if external {
        true
    } else {
        request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some_and(|previous| {
                if is_local_replay_cursor(previous) {
                    return true;
                }
                state.sessions.ancestry(previous).is_ok_and(|ancestry| {
                    ancestry
                        .last()
                        .is_none_or(|response| response.provider_id != "official")
                })
            })
    };
    if replays_local_history {
        state
            .require_authenticated_client(headers, request_query)
            .await?;
    }
    Ok(())
}

fn is_local_replay_cursor(response_id: &str) -> bool {
    response_id.starts_with(WARMUP_RESPONSE_ID_PREFIX)
        || response_id.starts_with(COMPACT_RESPONSE_ID_PREFIX)
}

#[cfg(test)]
fn prepare_replay(state: &AppState, request: &mut Value, target_provider: &str) -> Result<()> {
    prepare_replay_for_owner(state, request, target_provider, None)
}

fn prepare_replay_for_target(state: &AppState, request: &mut Value, target: &Target) -> Result<()> {
    prepare_replay_for_owner(
        state,
        request,
        &target.provider_id,
        target.provider_owner_id.as_ref(),
    )
}

/// The client's official backend defines the code-mode harness tools
/// (`exec`, `wait`) server-side; the desktop client never sends them for
/// regular turns. External models need the same definitions, so the router
/// keeps this faithful copy of the official `additional_tools` payload as
/// the fallback when no client-provided set was recorded.
const BUILTIN_HARNESS_TOOLS_JSON: &str = r#"[
  {
    "type": "custom",
    "name": "exec",
    "description": "Run JavaScript code to orchestrate/compose tool calls\n- Evaluates the provided JavaScript code in a fresh V8 isolate as an async module.\n- All nested tools are available on the global `tools` object, for example `await tools.exec_command(...)`. Tool names are exposed as normalized JavaScript identifiers, for example `await tools.mcp__ologs__get_profile(...)`.\n- Nested tool methods take either a string or an object as their input argument.\n- Nested tools return either an object or a string, based on the description.\n- Runs raw JavaScript -- no Node, no file system, no network access, no console.\n- Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.\n- You may optionally start the tool input with a first-line pragma like `// @exec: {\"yield_time_ms\": 10000, \"max_output_tokens\": 1000}`.\n- `yield_time_ms` asks `exec` to yield early if the script is still running. Defaults to 10000 ms.\n- `max_output_tokens` sets the token budget for direct `exec` results. Defaults to 10000 tokens.\n- When the JS code is fully evaluated, the isolate's lifetime ends and unawaited promises are silently discarded.\n\n- Global helpers:\n- `exit()`: Immediately ends the current script successfully (like an early return from the top level).\n- `text(value: string | number | boolean | undefined | null)`: Appends a text item. Non-string values are stringified with `JSON.stringify(...)` when possible.\n- `store(key: string, value: any)`: stores a serializable value under a string key for later `exec` calls in the same session.\n- `load(key: string)`: returns the stored value for a string key, or `undefined` if it is missing.\n- `notify(value: string | number | boolean | undefined | null)`: immediately injects an extra `custom_tool_call_output` for the current `exec` call. Values are stringified like `text(...)`.\n- `setTimeout(callback: () => void, delayMs?: number)`: schedules a callback to run later and returns a timeout id.\n- `clearTimeout(timeoutId?: number)`: cancels a timeout created by `setTimeout`.\n- `ALL_TOOLS`: metadata for the enabled nested tools as `{ name, description }` entries.\n- `yield_control()`: yields the accumulated output to the model immediately while the script keeps running.\n\nSome deferred nested tools may be omitted from this description. They are still available on the global `tools` object and listed in `ALL_TOOLS`.\nTo find one, filter `ALL_TOOLS` by `name` and `description`.\n\n### `apply_patch`\nThe `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.\n\nexec tool declaration:\n```ts\ndeclare const tools: { apply_patch(input: string): Promise<unknown>; };\n```\n\n### `shell_command`\nRuns a Powershell command (Windows) and returns its output.\n\nExamples of valid command strings:\n\n- ls -a (show hidden): \"Get-ChildItem -Force\"\n- recursive find by name: \"Get-ChildItem -Recurse -Filter *.py\"\n- recursive grep: \"Get-ChildItem -Path C:\\\\myrepo -Recurse | Select-String -Pattern 'TODO' -CaseSensitive\"\n- ps aux | grep python: \"Get-Process | Where-Object { $_.ProcessName -like '*python*' }\"\n- setting an env var: \"$env:FOO='bar'; echo $env:FOO\"\n- running an inline Python script: \"@'\\\\nprint('Hello, world!')\\\\n'@ | python -\"\n\nWindows safety rules:\n- Do not compose destructive filesystem commands across shells. Do not enumerate paths in PowerShell and then pass them to `cmd /c`, batch builtins, or another shell for deletion or moving. Use one shell end-to-end, prefer native PowerShell cmdlets such as `Remove-Item` / `Move-Item` with `-LiteralPath`, and avoid string-built shell commands for file operations.\n- Before any recursive delete or move on Windows, verify the resolved absolute target paths stay within the intended workspace or explicitly named target directory.\n\nexec tool declaration:\n```ts\ndeclare const tools: { shell_command(args: {\n  // Shell script to run in the user's default shell.\n  command: string;\n  // User-facing approval question for `require_escalated`; omit otherwise.\n  justification?: string;\n  // True runs with login shell semantics; false disables them. Defaults to true.\n  login?: boolean;\n  // Per-command sandbox override. Defaults to `use_default`; use `require_escalated` for unsandboxed execution.\n  sandbox_permissions?: \"use_default\" | \"require_escalated\";\n  // Maximum command runtime. Defaults to 10000 ms.\n  timeout_ms?: number;\n  // Working directory for the command. Defaults to the turn cwd.\n  workdir?: string;\n}): Promise<unknown>; };\n```\n\n### `update_plan`\nUpdates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\n\nexec tool declaration:\n```ts\ndeclare const tools: { update_plan(args: {\n  // Optional explanation for this plan update.\n  explanation?: string;\n  // The list of steps\n  plan: Array<{\n  // Step status.\n  status: \"pending\" | \"in_progress\" | \"completed\";\n  // Task step text.\n  step: string;\n}>;\n}): Promise<unknown>; };\n```\n\n### `view_image`\nView a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk.\n\nexec tool declaration:\n```ts\ndeclare const tools: { view_image(args: {\n  // Image detail level. Defaults to `high`; use `original` to preserve exact resolution.\n  detail?: \"high\" | \"original\";\n  // Local filesystem path to an image file.\n  path: string;\n}): Promise<{\n  detail: \"high\" | \"original\";\n  image_url: string;\n}>; };\n```",
    "format": {
      "type": "grammar",
      "syntax": "lark",
      "definition": "\nstart: pragma_source | plain_source\npragma_source: PRAGMA_LINE NEWLINE SOURCE\nplain_source: SOURCE\n\nPRAGMA_LINE: /[ \\t]*\\/\\/ @exec:[^\\r\\n]*/\nNEWLINE: /\\r?\\n/\nSOURCE: /[\\s\\S]+/\n"
    }
  },
  {
    "type": "function",
    "name": "wait",
    "description": "Waits on a yielded `exec` cell and returns new output or completion.\n- Use `wait` only after `exec` returns `Script running with cell ID ...`.\n- `cell_id` identifies the running `exec` cell to resume.\n- `yield_time_ms` controls how long to wait for more output before yielding again. Defaults to 10000 ms.\n- `max_tokens` limits how much new output this wait call returns. Defaults to 10000 tokens.\n- `terminate: true` stops the running cell; false or omitted waits for output.\n- `wait` returns only the new output since the last yield, or the final completion or termination result for that cell.\n- If the cell is still running, `wait` may yield again with the same `cell_id`.\n- If the cell has already finished, `wait` returns the completed result and closes the cell.",
    "strict": false,
    "parameters": {
      "type": "object",
      "properties": {
        "cell_id": {"type": "string", "description": "Identifier of the running exec cell."},
        "max_tokens": {"type": "number", "description": "Output token budget for this wait call. Defaults to 10000 tokens."},
        "terminate": {"type": "boolean", "description": "True stops the running exec cell; false or omitted waits for output."},
        "yield_time_ms": {"type": "number", "description": "Wait before yielding more output. Defaults to 10000 ms."}
      },
      "required": ["cell_id"],
      "additionalProperties": false
    }
  }
]"#;

fn builtin_harness_tools() -> Vec<Value> {
    serde_json::from_str(BUILTIN_HARNESS_TOOLS_JSON)
        .expect("builtin harness tools payload is valid JSON")
}

fn has_usable_tool_definitions(tools: &[Value]) -> bool {
    tools.iter().any(|tool| {
        matches!(
            tool.get("type").and_then(Value::as_str),
            Some("custom" | "function")
        ) && tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty())
    })
}

/// Injects the harness tool definitions into an external turn.
///
/// The desktop client relies on Responses server-side state: the official
/// backend defines the code-mode `exec`/`wait` tools itself and regular
/// turns arrive with no tool definitions at all. Without them the external
/// model can only announce commands as text. A recorded client-provided set
/// wins when available; otherwise the builtin official definitions are used.
fn ensure_external_harness_tools(
    state: &AppState,
    request: &mut Value,
    target: &Target,
) -> Result<()> {
    if !target.external {
        return Ok(());
    }
    let Some(object) = request.as_object() else {
        return Ok(());
    };
    let has_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| has_usable_tool_definitions(tools));
    let input_has_definitions =
        object
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("additional_tools")
                        && item
                            .get("tools")
                            .and_then(Value::as_array)
                            .is_some_and(|tools| has_usable_tool_definitions(tools))
                })
            });
    if has_tools || input_has_definitions {
        return Ok(());
    }
    let tools = state
        .sessions
        .latest_harness_tools()?
        .unwrap_or_else(builtin_harness_tools);
    tracing::info!(
        tools = tools.len(),
        "injecting harness tool definitions into external turn"
    );
    let Some(object) = request.as_object_mut() else {
        return Ok(());
    };
    // Responses allows a bare string as `input`; a turn without a
    // previous_response_id can still carry that shorthand here, so
    // normalize it instead of discarding the turn's only message.
    if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
        items.insert(
            0,
            json!({"type": "additional_tools", "role": "developer", "tools": tools}),
        );
    } else {
        let text = object
            .get("input")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut items =
            vec![json!({"type": "additional_tools", "role": "developer", "tools": tools})];
        if let Some(text) = text {
            items.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            }));
        }
        object.insert("input".into(), Value::Array(items));
    }
    Ok(())
}

fn prepare_replay_for_owner(
    state: &AppState,
    request: &mut Value,
    target_provider: &str,
    target_owner: Option<&ProviderOwnerId>,
) -> Result<()> {
    let previous = request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let original_current = input_items(request)?;
    let current_resets_history = original_current
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"));
    let current =
        portable_input_items_for_owner(state, original_current, target_provider, target_owner)?;
    let Some(previous) = previous else {
        if current != input_items(request)? {
            request
                .as_object_mut()
                .ok_or_else(|| RouterError::bad_request("request must be an object"))?
                .insert("input".into(), Value::Array(current));
        }
        return Ok(());
    };
    let ancestry = match state.sessions.ancestry(&previous) {
        Ok(ancestry) => ancestry,
        Err(error) if target_provider == "official" && !is_local_replay_cursor(&previous) => {
            // A native ChatGPT previous id may predate the local router. It is
            // safe to leave that opaque official chain on the official backend.
            if current != input_items(request)? {
                request
                    .as_object_mut()
                    .ok_or_else(|| RouterError::bad_request("request must be an object"))?
                    .insert("input".into(), Value::Array(current));
            }
            let _ = error;
            return Ok(());
        }
        Err(_) => {
            return Err(RouterError::bad_request(
                "previous_response_id is not in the local history; cross-provider continuation cannot preserve context",
            ));
        }
    };
    if target_provider == "official"
        && previous.starts_with(WARMUP_RESPONSE_ID_PREFIX)
        && apply_official_warmup_cursor(
            state,
            request,
            &ancestry,
            &current,
            current_resets_history,
            target_owner,
        )?
    {
        return Ok(());
    }
    // A warmup after an external response has no native official cursor, so it
    // falls through to the normal cross-provider replay planner.
    if target_provider == "official"
        && !is_local_replay_cursor(&previous)
        && ancestry
            .last()
            .is_some_and(|response| response.provider_id == "official")
    {
        // Native Responses continuation carries opaque server-side state that
        // cannot be reconstructed from visible items. Preserve same-provider
        // official requests exactly, including previous_response_id.
        if current != input_items(request)? {
            request
                .as_object_mut()
                .ok_or_else(|| RouterError::bad_request("request must be an object"))?
                .insert("input".into(), Value::Array(current));
        }
        return Ok(());
    }
    // Model switches should stay lossless: replay the recorded pre-compaction
    // history too whenever it fits the external model's window, and only fall
    // back to the compaction boundary when the context is genuinely full.
    if try_lossless_external_replay(state, request, &previous, target_provider, target_owner)? {
        return Ok(());
    }
    let mut replay = if current_resets_history {
        Vec::new()
    } else if let Some(owner) = target_owner {
        state
            .sessions
            .replay_items_for_owner(&previous, target_provider, owner)?
    } else {
        state.sessions.replay_items(&previous, target_provider)?
    };
    replay.extend(current);
    let object = request
        .as_object_mut()
        .ok_or_else(|| RouterError::bad_request("request must be an object"))?;
    object.insert("input".into(), Value::Array(replay));
    object.remove("previous_response_id");
    Ok(())
}

/// Rewrites the request to the lossless full replay when the recorded history
/// (including pre-compaction turns) fits the external model's context window.
/// Returns `true` when the rewrite happened.
fn try_lossless_external_replay(
    state: &AppState,
    request: &mut Value,
    previous: &str,
    target_provider: &str,
    target_owner: Option<&ProviderOwnerId>,
) -> Result<bool> {
    if target_provider == "official" {
        return Ok(false);
    }
    // A compaction item in the current input resets the history; the boundary
    // planner owns that semantics, so never short-circuit it here.
    if input_items(request)?
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
    {
        return Ok(false);
    }
    let Some(window) = external_context_window(&state.config, target_public_model(request)) else {
        return Ok(false);
    };
    let full_ancestry =
        state
            .sessions
            .replay_items_full_for_owner(previous, target_provider, target_owner);
    let full_current =
        portable_input_items_full(state, input_items(request)?, target_provider, target_owner);
    let (Ok(full_ancestry), Ok(full_current)) = (full_ancestry, full_current) else {
        return Ok(false);
    };
    let mut combined = full_ancestry;
    combined.extend(full_current);
    if !full_replay_fits(&combined, window) {
        tracing::info!(
            model = target_public_model(request),
            window,
            "full replay exceeds the external context window; using the compaction boundary"
        );
        return Ok(false);
    }
    tracing::info!(
        model = target_public_model(request),
        window,
        "lossless cross-provider replay fits the external context window"
    );
    let object = request
        .as_object_mut()
        .ok_or_else(|| RouterError::bad_request("request must be an object"))?;
    object.insert("input".into(), Value::Array(combined));
    object.remove("previous_response_id");
    Ok(true)
}

fn target_public_model(request: &Value) -> &str {
    request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn external_context_window(config: &RouterConfig, public_model: &str) -> Option<u64> {
    if public_model.is_empty() {
        return None;
    }
    config
        .models
        .iter()
        .find(|model| model.id == public_model)
        .and_then(|model| model.context_window)
        .filter(|window| *window > 0)
}

/// Rough token estimate for replay items. CJK characters tokenize at roughly
/// 1.5-2 tokens each (cl100k-class tokenizers), while ASCII averages ~4 bytes
/// per token, so counting non-ASCII separately keeps the estimate on the safe
/// side for mixed Chinese/English history.
fn estimated_tokens(items: &[Value]) -> u64 {
    let mut ascii_bytes: u64 = 0;
    let mut non_ascii_chars: u64 = 0;
    for item in items {
        for byte in item.to_string().bytes() {
            if byte.is_ascii() {
                ascii_bytes += 1;
            } else {
                non_ascii_chars += 1;
            }
        }
    }
    ascii_bytes / 4 + non_ascii_chars * 2
}

/// Full replay must leave room for the answer and estimation error, so it is
/// only used below 70% of the configured window.
fn full_replay_fits(items: &[Value], window: u64) -> bool {
    estimated_tokens(items) * 10 <= window * 7
}

fn apply_official_warmup_cursor(
    state: &AppState,
    request: &mut Value,
    ancestry: &[ResponseRecord],
    current: &[Value],
    current_resets_history: bool,
    target_owner: Option<&ProviderOwnerId>,
) -> Result<bool> {
    let warmup_start = ancestry
        .iter()
        .rposition(|response| !response.id.starts_with(WARMUP_RESPONSE_ID_PREFIX))
        .map_or(0, |index| index + 1);
    if warmup_start >= ancestry.len() {
        return Ok(false);
    }
    let cursor = if warmup_start > 0 {
        let predecessor = &ancestry[warmup_start - 1];
        if predecessor.provider_id != "official" || is_local_replay_cursor(&predecessor.id) {
            return Ok(false);
        }
        Some(predecessor.id.clone())
    } else {
        ancestry[warmup_start].previous_response_id.clone()
    };
    let mut replay = Vec::new();
    for warmup in &ancestry[warmup_start..] {
        if warmup.provider_id != "official" || !warmup.id.starts_with(WARMUP_RESPONSE_ID_PREFIX) {
            return Ok(false);
        }
        let resets = warmup
            .input
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"));
        let sanitized =
            portable_input_items_for_owner(state, warmup.input.clone(), "official", target_owner)?;
        if resets {
            replay.clear();
        }
        replay.extend(sanitized);
    }
    if current_resets_history {
        replay.clear();
    }
    replay.extend(current.iter().cloned());
    let object = request
        .as_object_mut()
        .ok_or_else(|| RouterError::bad_request("request must be an object"))?;
    object.insert("input".into(), Value::Array(replay));
    if let Some(cursor) = cursor {
        object.insert("previous_response_id".into(), Value::String(cursor));
    } else {
        object.remove("previous_response_id");
    }
    Ok(true)
}

#[cfg(test)]
fn portable_input_items(
    state: &AppState,
    items: Vec<Value>,
    target_provider: &str,
) -> Result<Vec<Value>> {
    portable_input_items_for_owner(state, items, target_provider, None)
}

fn portable_input_items_for_owner(
    state: &AppState,
    items: Vec<Value>,
    target_provider: &str,
    target_owner: Option<&ProviderOwnerId>,
) -> Result<Vec<Value>> {
    portable_input_items_internal(state, items, target_provider, target_owner, false)
}

/// Full-replay variant for external targets: compaction items are dropped
/// instead of mapped, because the pre-compaction history is replayed verbatim
/// by the caller.
fn portable_input_items_full(
    state: &AppState,
    items: Vec<Value>,
    target_provider: &str,
    target_owner: Option<&ProviderOwnerId>,
) -> Result<Vec<Value>> {
    portable_input_items_internal(state, items, target_provider, target_owner, true)
}

fn portable_input_items_internal(
    state: &AppState,
    items: Vec<Value>,
    target_provider: &str,
    target_owner: Option<&ProviderOwnerId>,
    full: bool,
) -> Result<Vec<Value>> {
    let mut portable = Vec::with_capacity(items.len());
    for mut item in items {
        let is_compaction = item.get("type").and_then(Value::as_str) == Some("compaction");
        if full && is_compaction {
            continue;
        }
        if !is_compaction || target_provider == "official" {
            if is_compaction {
                portable.clear();
                portable.push(item);
                continue;
            }
        } else {
            let mapping = state.sessions.compaction_for_item(&item)?;
            portable.clear();
            match mapping {
                Some(mapping)
                    if target_owner
                        .is_some_and(|owner| mapping.source_owner_id.as_ref() == Some(owner)) =>
                {
                    portable.push(item);
                }
                Some(mapping) => {
                    portable.push(portable_compaction_summary(&mapping.portable_summary));
                }
                None => {
                    // Official-only compaction with no locally recorded portable
                    // summary: the encrypted payload cannot cross providers, but
                    // refusing the whole switch strands the conversation. Replay
                    // a neutral note instead; everything after the compaction
                    // boundary still carries the recent context.
                    tracing::warn!(
                        "official compaction item without a portable summary; \
                         continuing the provider switch with a neutral note"
                    );
                    portable.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "[context note] Earlier conversation history was compacted by the official provider and its summary is not available when switching to this external model.",
                        }],
                    }));
                }
            }
            continue;
        }

        let owner = item
            .pointer("/provider_metadata/cmr_provider_owner_id")
            .and_then(Value::as_str);
        let exact_owner = target_owner.is_some_and(|target| {
            owner == Some(target.as_str()) && ProviderOwnerId::parse(target.as_str()).is_ok()
        });
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            // Direct input is not tied to a recorded response. Preserve private
            // external reasoning only when the router stamped it for this exact
            // provider instance. Native official encrypted reasoning has no CMR
            // metadata and remains valid only for the official provider.
            if !exact_owner {
                continue;
            }
        }
        if item.get("provider_metadata").is_some() && !exact_owner {
            if let Some(object) = item.as_object_mut() {
                object.remove("provider_metadata");
            }
        }
        portable.push(item);
    }
    Ok(portable)
}

#[cfg(test)]
fn canonical_current_input_for_storage(
    state: &AppState,
    items: Vec<Value>,
    target_provider: &str,
) -> Result<Vec<Value>> {
    let mut items = portable_input_items(state, items, target_provider)?;
    // provider_metadata on request input is client-controlled.  It may guide
    // same-turn filtering, but it must never become trusted stored provenance.
    // Provider-owned metadata is stamped only on decoded provider output.
    for item in &mut items {
        if let Some(object) = item.as_object_mut() {
            object.remove("provider_metadata");
        }
    }
    Ok(items)
}

fn canonical_current_input_for_target(
    state: &AppState,
    items: Vec<Value>,
    target: &Target,
) -> Result<Vec<Value>> {
    let mut items = portable_input_items_for_owner(
        state,
        items,
        &target.provider_id,
        target.provider_owner_id.as_ref(),
    )?;
    for item in &mut items {
        if let Some(object) = item.as_object_mut() {
            object.remove("provider_metadata");
        }
    }
    Ok(items)
}

fn portable_compaction_summary(summary: &str) -> Value {
    // A provider-neutral continuation summary is control context, never
    // assistant speech and never a fabricated compaction item.
    json!({
        "type": "message",
        "role": "developer",
        "content": [{"type":"input_text","text":summary}],
        "metadata": {"cmr_portable_compaction": true}
    })
}

fn ensure_compaction_tools_closed(
    state: &AppState,
    previous: Option<&str>,
    current_input: &[Value],
) -> Result<()> {
    let mut pending = BTreeSet::new();
    if let Some(previous) = previous {
        if let Ok(ancestry) = state.sessions.ancestry(previous) {
            for response in ancestry {
                update_pending_function_calls(&mut pending, &response.input)?;
                update_pending_function_calls(&mut pending, &response.output)?;
            }
        }
    }
    update_pending_function_calls(&mut pending, current_input)?;
    if !pending.is_empty() {
        return Err(RouterError::bad_request(format!(
            "cannot compact while {} function call(s) are awaiting function_call_output",
            pending.len()
        )));
    }
    Ok(())
}

fn update_pending_function_calls(pending: &mut BTreeSet<String>, items: &[Value]) -> Result<()> {
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("compaction") => pending.clear(),
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .or_else(|| {
                        item.get("id")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                    })
                    .ok_or_else(|| {
                        RouterError::bad_request(
                            "function_call item must contain a non-empty call_id or id before compaction",
                        )
                    })?;
                pending.insert(call_id.to_owned());
            }
            Some("function_call_output") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        RouterError::bad_request(
                            "function_call_output item must contain a non-empty call_id before compaction",
                        )
                    })?;
                pending.remove(call_id);
            }
            _ => {}
        }
    }
    Ok(())
}

fn input_items(request: &Value) -> Result<Vec<Value>> {
    match request.get("input") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
        ]),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(RouterError::bad_request("input must be a string or array")),
    }
}

fn session_id(state: &AppState, previous: Option<&str>, request: &Value) -> String {
    if let Some(previous) = previous {
        if let Ok(chain) = state.sessions.ancestry(previous) {
            if let Some(first) = chain.first() {
                return first.session_id.clone();
            }
        }
    }
    request
        .pointer("/metadata/cmr_session_id")
        .and_then(Value::as_str)
        .map_or_else(|| format!("session_{}", Uuid::new_v4()), str::to_owned)
}

fn endpoint(target: &Target, adapter: &dyn ProviderAdapter, stream: bool) -> String {
    format!(
        "{}{}",
        target.preset.base_url.trim_end_matches('/'),
        adapter.request_path(&target.upstream_model, stream)
    )
}

fn external_request(
    state: &AppState,
    target: &Target,
    mut builder: reqwest::RequestBuilder,
    body: &Value,
) -> Result<reqwest::RequestBuilder> {
    builder = builder
        .header(header::CONTENT_TYPE, "application/json")
        .json(body);
    for (name, value) in &target.preset.headers {
        builder = builder.header(name, value);
    }
    if target.preset.auth != AuthStyle::None {
        let reference = target.secret_ref.as_deref().ok_or_else(|| {
            RouterError::unauthorized(format!("provider {} has no secret_ref", target.provider_id))
        })?;
        let reference = SecretRef::parse(reference)?;
        let secret = state.credentials.get(&reference)?.ok_or_else(|| {
            RouterError::unauthorized(format!(
                "credential {} is not available",
                reference.account()
            ))
        })?;
        builder = match target.preset.auth {
            AuthStyle::Bearer => builder.bearer_auth(secret),
            AuthStyle::XApiKey => builder.header("x-api-key", secret),
            AuthStyle::GoogleApiKey => builder.header("x-goog-api-key", secret),
            AuthStyle::None => builder,
        };
    }
    Ok(builder)
}

async fn official_json_value(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    body: Value,
) -> Result<Value> {
    let url = format!(
        "{}{}",
        state.config.official_base_url.trim_end_matches('/'),
        path
    );
    let upstream = state
        .client
        .post(url)
        .headers(forward_headers(headers, true, false))
        .json(&body)
        .send()
        .await?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(RouterError::upstream(
            status,
            "official ChatGPT backend rejected the request",
        ));
    }
    let value: Value = upstream.json().await?;
    Ok(value)
}

fn validate_compaction_response(response: &Value) -> Result<&Value> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "official compaction response has no output array",
            )
        })?;
    if output.len() != 1 || output[0].get("type").and_then(Value::as_str) != Some("compaction") {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            format!(
                "official compaction must return exactly one compaction item; got {}",
                output.len()
            ),
        ));
    }
    if output[0]
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            "official compaction item has empty encrypted_content",
        ));
    }
    Ok(&output[0])
}

/// Removes a Remote Compaction V2 control item from a normal Responses request.
/// The control item is accepted only as the final, fieldless input item so it
/// can never be mistaken for durable conversation history.
fn take_compaction_trigger(request: &mut Value) -> Result<bool> {
    let Some(input) = request.get_mut("input") else {
        return Ok(false);
    };
    let Some(items) = input.as_array_mut() else {
        return Ok(false);
    };
    let trigger_positions = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if trigger_positions.is_empty() {
        return Ok(false);
    }
    let valid = trigger_positions.as_slice() == [items.len().saturating_sub(1)]
        && items
            .last()
            .and_then(Value::as_object)
            .is_some_and(|item| item.len() == 1);
    if !valid {
        return Err(RouterError::bad_request(
            "compaction_trigger must be the final input item and contain no other fields",
        ));
    }
    items.pop();
    Ok(true)
}

fn normalize_compaction_response(response: &Value, model: &str, item: &Value) -> Value {
    json!({
        // A compact response is a locally replayable boundary, never a native
        // Responses previous_response_id, even when the upstream includes an id.
        "id": format!("{COMPACT_RESPONSE_ID_PREFIX}{}", Uuid::new_v4()),
        "object": "response",
        "created_at": response.get("created_at").and_then(Value::as_i64)
            .unwrap_or_else(|| Utc::now().timestamp()),
        "status": "completed",
        "model": model,
        "output": [item.clone()]
    })
}

fn compaction_events(response: &Value) -> Vec<Value> {
    let item = response
        .get("output")
        .and_then(Value::as_array)
        .and_then(|output| output.first())
        .cloned()
        .expect("normalized compaction response always has one output item");
    let mut in_progress = response.clone();
    in_progress["status"] = Value::String("in_progress".into());
    in_progress["output"] = Value::Array(Vec::new());
    vec![
        json!({"type":"response.created","sequence_number":0,"response":in_progress.clone()}),
        json!({"type":"response.in_progress","sequence_number":1,"response":in_progress}),
        json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":item.clone()}),
        json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":item}),
        json!({"type":"response.completed","sequence_number":4,"response":response}),
    ]
}

fn compaction_sse_response(response: &Value) -> Response {
    let mut body = String::new();
    for event in compaction_events(response) {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from(body))
        .expect("static compaction stream headers are valid")
}

fn response_error_event(code: &str, message: &str, sequence_number: u64) -> Value {
    tracing::warn!(code, message, "sending response error event to client");
    json!({
        "type": "error",
        "code": code,
        "message": message,
        "param": null,
        "sequence_number": sequence_number
    })
}

fn stream_terminal(event: &Value) -> Option<StreamTerminal> {
    match event.get("type").and_then(Value::as_str) {
        Some("response.completed") => Some(StreamTerminal::Completed),
        Some("response.incomplete") => Some(StreamTerminal::Incomplete),
        Some("response.failed" | "error") => Some(StreamTerminal::Failed),
        _ => None,
    }
}

pub(crate) fn forward_headers(headers: &HeaderMap, official: bool, catalog: bool) -> HeaderMap {
    let mut result = HeaderMap::new();
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        let allowed = if official {
            // The official backend is the only destination allowed to receive
            // ChatGPT credentials. Keep this as an allowlist: an inbound
            // provider key or an arbitrary extension header must never hitch a
            // ride to chatgpt.com merely because it is not on a denylist.
            matches!(
                lower.as_str(),
                "accept"
                    | "authorization"
                    | "content-type"
                    | "cookie"
                    | "idempotency-key"
                    | "originator"
                    | "user-agent"
            ) || lower.starts_with("chatgpt-")
                || lower.starts_with("openai-")
                || lower.starts_with("x-openai-")
        } else {
            // External adapters obtain authentication exclusively from the
            // configured credential store. Only content-negotiation metadata
            // may cross this boundary from an inbound request.
            matches!(lower.as_str(), "accept" | "content-type" | "user-agent")
        };
        if !allowed {
            continue;
        }
        if catalog && lower == "accept" {
            continue;
        }
        result.append(name.clone(), value.clone());
    }
    result
}

async fn send_sse(
    sender: &mpsc::Sender<std::result::Result<Bytes, Infallible>>,
    value: &Value,
) -> std::result::Result<(), ()> {
    sender
        .send(Ok(Bytes::from(format!("data: {value}\n\n"))))
        .await
        .map_err(|_| ())
}

fn parse_sse_data(data: &str) -> Result<Option<Value>> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str(data).map(Some).map_err(|_| {
        RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            "upstream SSE data event contained malformed JSON",
        )
    })
}

// Responses stream validation is intentionally one contiguous state machine so
// cross-event ordering and terminal invariants remain auditable together.
#[allow(clippy::too_many_lines)]
fn tap_event(event: &Value, response_id: &mut String, state: &mut StreamTapState) -> Result<()> {
    if state.terminal_status.is_some() {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            "upstream emitted an event after a terminal Responses event",
        ));
    }
    if let Some(id) = event
        .pointer("/response/id")
        .and_then(Value::as_str)
        .or_else(|| event.get("response_id").and_then(Value::as_str))
    {
        id.clone_into(response_id);
    }
    if event.get("type").and_then(Value::as_str) == Some("response.output_item.done") {
        let output_index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "response.output_item.done has no valid output_index",
                )
            })?;
        let item = event.get("item").ok_or_else(|| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "response.output_item.done has no item",
            )
        })?;
        if state.done_items.contains_key(&output_index) {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "response.output_item.done repeated an output_index",
            ));
        }
        let item_id = match item.get("id") {
            None => None,
            Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
            Some(_) => {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "response.output_item.done item id must be a non-empty string",
                ));
            }
        };
        if let Some(item_id) = &item_id {
            if state.done_item_ids.contains_key(item_id) {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "response.output_item.done repeated an item id",
                ));
            }
        }
        let mut candidate_done_items = state.done_items.clone();
        candidate_done_items.insert(output_index, item.clone());
        let candidate_items = candidate_done_items.values().cloned().collect::<Vec<_>>();
        // Validate a function call's stable identity and executable fields before
        // the done event can be forwarded. This also rejects a second item with
        // the same call_id/id before a client has a chance to execute it twice.
        canonical_function_calls(&candidate_items)?;
        state.done_items.insert(output_index, item.clone());
        if let Some(item_id) = item_id {
            state.done_item_ids.insert(item_id, output_index);
        }
    }
    let terminal_status = match event.get("type").and_then(Value::as_str) {
        Some("response.completed") => Some(ResponseStatus::Completed),
        Some("response.incomplete") => Some(ResponseStatus::Incomplete),
        _ => None,
    };
    if let Some(terminal_status) = terminal_status {
        let items = event
            .pointer("/response/output")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "terminal Responses event has no response.output array",
                )
            })?;
        if state.done_items.len() != items.len()
            || items
                .iter()
                .enumerate()
                .any(|(index, item)| state.done_items.get(&(index as u64)) != Some(item))
        {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "terminal response.output does not exactly match ordered output_item.done events",
            ));
        }
        canonical_function_calls(items)?;
        let declared_status = event
            .pointer("/response/status")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "terminal Responses event has no response.status",
                )
            })?;
        let expected_status = match terminal_status {
            ResponseStatus::Completed => "completed",
            ResponseStatus::Incomplete => "incomplete",
            ResponseStatus::InProgress => unreachable!("only terminal statuses are selected"),
        };
        if declared_status != expected_status {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "terminal Responses event type and response.status disagree",
            ));
        }
        let incomplete_details = match event.pointer("/response/incomplete_details") {
            None | Some(Value::Null) => None,
            Some(details @ Value::Object(_)) => Some(details.clone()),
            Some(_) => {
                return Err(RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "terminal response.incomplete_details must be an object or null",
                ));
            }
        };
        if terminal_status == ResponseStatus::Completed && incomplete_details.is_some() {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "response.completed cannot carry incomplete_details",
            ));
        }
        state.output.clone_from(items);
        state.terminal_status = Some(terminal_status);
        state.incomplete_details = incomplete_details;
    }
    Ok(())
}

fn stream_batch_completed(state: &StreamTapState) -> bool {
    state.terminal_status.is_some()
}

fn canonical_function_calls(items: &[Value]) -> Result<Vec<CanonicalFunctionCall>> {
    let mut call_ids = BTreeSet::new();
    let mut item_ids = BTreeSet::new();
    let mut calls = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if call_id.is_none() && id.is_none() {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "streamed function_call item has no call_id or id",
            ));
        }
        if call_id
            .as_ref()
            .is_some_and(|call_id| !call_ids.insert(call_id.clone()))
            || id.as_ref().is_some_and(|id| !item_ids.insert(id.clone()))
        {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "streamed function_call items contain a duplicate call_id or id",
            ));
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "streamed function_call item has no non-empty name",
                )
            })?
            .to_owned();
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RouterError::upstream(
                    StatusCode::BAD_GATEWAY,
                    "streamed function_call item has no string arguments",
                )
            })?
            .to_owned();
        if !serde_json::from_str::<Value>(&arguments).is_ok_and(|value| value.is_object()) {
            return Err(RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "streamed function_call item arguments must encode a JSON object",
            ));
        }
        calls.push(CanonicalFunctionCall {
            call_id,
            id,
            name,
            arguments,
        });
    }
    Ok(calls)
}

fn visible_text(response: &Value) -> String {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .flat_map(|item| {
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn completed_visible_summary(response: &Value, operation: &str) -> Result<String> {
    let (_, _, status, _) = validate_nonstream_terminal(response)?;
    if status != ResponseStatus::Completed {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            format!("{operation} did not complete"),
        ));
    }
    let summary = visible_text(response);
    if summary.trim().is_empty() {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            format!("{operation} returned an empty summary"),
        ));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};
    use cmr_storage::{
        ConfigInstanceId, MemoryCredentialStore, ModelConfig, ProviderConfig, StateStore,
    };
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    fn test_owner(provider: &str, generation: &str) -> ProviderOwnerId {
        let instance = ConfigInstanceId::parse(&"a".repeat(64)).expect("test config instance");
        ProviderOwnerId::for_credential_generation(
            &instance,
            provider,
            "https://example.invalid/v1",
            generation,
        )
        .expect("test provider owner")
    }

    async fn assert_stream_transport_stamps_exact_owner(transport: &str) {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        state
            .official_models
            .write()
            .await
            .push(json!({"slug":"gpt-test"}));
        let owner = test_owner("official", &format!("{transport}-generation-a"));
        let rotated_owner = test_owner("official", &format!("{transport}-generation-b"));
        let mut target = resolve_target(&state, &json!({"model":"gpt-test"}))
            .await
            .expect("official target");
        target.provider_owner_id = Some(owner.clone());

        let response_id = format!("resp_{transport}_owner");
        let input = vec![json!({
            "type":"message",
            "role":"user",
            "content":"retain same-owner reasoning"
        })];
        let reasoning = json!({
            "type":"reasoning",
            "id":format!("rs_{transport}"),
            "encrypted_content":format!("opaque-{transport}-reasoning")
        });
        let created_at = Utc::now();
        let mut seen_response_id = "synthetic".to_owned();
        let mut tap = StreamTapState::default();
        let mut begin_record = None;
        for event in [
            json!({
                "type":"response.created",
                "response":{"id":response_id,"status":"in_progress","output":[]}
            }),
            json!({
                "type":"response.output_item.done",
                "response_id":response_id,
                "output_index":0,
                "item":reasoning
            }),
            json!({
                "type":"response.completed",
                "response":{"id":response_id,"status":"completed","output":[reasoning]}
            }),
        ] {
            tap_event(&event, &mut seen_response_id, &mut tap).expect("valid stream event");
            persist_stream_event_before_delivery(
                &state,
                &event,
                &seen_response_id,
                &mut begin_record,
                &target,
                &input,
                None,
                &format!("session_{transport}"),
                created_at,
            )
            .expect("persist before delivery");
        }

        let mut record = begin_record.expect("response.created began persistence");
        assert_eq!(record.provider_owner_id.as_ref(), Some(&owner));
        record.output = tap.output;
        record.status = tap.terminal_status.expect("terminal status");
        record.incomplete_details = tap.incomplete_details;
        state
            .sessions
            .record_response(&record)
            .expect("commit terminal response");

        let same_owner = state
            .sessions
            .replay_items_for_owner(&response_id, "official", &owner)
            .expect("same-owner replay");
        let same_owner_json = serde_json::to_string(&same_owner).expect("serialize replay");
        assert!(same_owner_json.contains(&format!("opaque-{transport}-reasoning")));

        let rotated = state
            .sessions
            .replay_items_for_owner(&response_id, "official", &rotated_owner)
            .expect("rotated-owner replay");
        let rotated_json = serde_json::to_string(&rotated).expect("serialize replay");
        assert!(!rotated_json.contains(&format!("opaque-{transport}-reasoning")));
    }

    #[tokio::test]
    async fn http_sse_stream_stamps_owner_for_private_reasoning_replay() {
        // HTTP SSE and native official WebSocket both persist each accepted
        // event through `persist_stream_event_before_delivery`.
        assert_stream_transport_stamps_exact_owner("http_sse").await;
    }

    #[tokio::test]
    async fn native_official_websocket_stamps_owner_for_private_reasoning_replay() {
        assert_stream_transport_stamps_exact_owner("native_ws").await;
    }

    #[tokio::test]
    async fn target_resolution_rejects_unknown_models_after_catalog_cache() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        state
            .official_models
            .write()
            .await
            .push(json!({"slug":"gpt-test"}));

        let official = resolve_target(&state, &json!({"model":"gpt-test"}))
            .await
            .expect("catalogued official model");
        assert!(!official.external);

        let error =
            match resolve_target(&state, &json!({"model":"model-that-does-not-exist"})).await {
                Ok(_) => panic!("unknown model must fail closed"),
                Err(error) => error,
            };
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("unknown model"));
    }

    #[tokio::test]
    async fn disabled_external_provider_cannot_shadow_cached_official_model() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "disabled-external".into(),
            preset: "custom-compatible".into(),
            base_url: Some("https://example.invalid/v1".into()),
            secret_ref: None,
            enabled: false,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "gpt-x".into(),
            display_name: "External GPT X".into(),
            provider: "disabled-external".into(),
            upstream_model: "external-gpt-x".into(),
            order: 0,
            enabled: true,
            context_window: None,
            max_output_tokens: None,
        });
        let state = AppState::with_credentials(
            config,
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        state
            .official_models
            .write()
            .await
            .push(json!({"slug":"gpt-x"}));

        let target = resolve_target(&state, &json!({"model":"gpt-x"}))
            .await
            .expect("cached official model must win");

        assert!(!target.external);
        assert_eq!(target.provider_id, "official");
        assert_eq!(target.public_model, "gpt-x");
        assert_eq!(target.upstream_model, "gpt-x");
    }

    #[test]
    fn official_websocket_warmup_is_handled_locally() {
        let official = Target {
            provider_id: "official".into(),
            public_model: "gpt-test".into(),
            upstream_model: "gpt-test".into(),
            preset: preset_by_id("openai").expect("official preset"),
            max_output_tokens: None,
            external: false,
            secret_ref: None,
            provider_owner_id: None,
        };
        assert!(!should_relay_official_websocket(
            &official,
            &json!({"generate":false})
        ));
        assert!(should_relay_official_websocket(
            &official,
            &json!({"generate":true})
        ));
    }

    async fn zhipu_target(model_limit: Option<u64>) -> Target {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "zhipu-test".into(),
            preset: "zhipu".into(),
            base_url: None,
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "glm-test".into(),
            display_name: "GLM Test".into(),
            provider: "zhipu-test".into(),
            upstream_model: "glm-5.2".into(),
            order: 0,
            enabled: true,
            context_window: None,
            max_output_tokens: model_limit,
        });
        let state = AppState::with_credentials(
            config,
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        resolve_target(&state, &json!({"model":"glm-test"}))
            .await
            .expect("zhipu target")
    }

    #[tokio::test]
    async fn external_max_output_tokens_are_strictly_validated_and_forwarded() {
        let target = zhipu_target(None).await;
        let limit = 131_072_u64;
        assert_eq!(target.max_output_tokens, Some(limit));

        for invalid in [json!(0), json!(-1), json!(1.5), json!("1024")] {
            let request = json!({"model":"glm-test","max_output_tokens":invalid});
            let error = validate_external_max_output_tokens(&request, &target)
                .expect_err("non-positive or non-integer values must fail locally");
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert_eq!(error.code, "invalid_request");
        }

        let allowed = json!({
            "model":"glm-test",
            "input":[{"type":"message","role":"user","content":"hello"}],
            "max_output_tokens":limit
        });
        validate_external_max_output_tokens(&allowed, &target).expect("limit is inclusive");
        let adapter = adapter_for_preset(target.preset.clone());
        let converted = adapter
            .encode_request(&allowed)
            .expect("encode zhipu request");
        assert_eq!(converted["max_tokens"], limit);

        let over = json!({"model":"glm-test","max_output_tokens":limit + 1});
        let error = validate_external_max_output_tokens(&over, &target)
            .expect_err("limit plus one must fail locally");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);

        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        state
            .official_models
            .write()
            .await
            .push(json!({"slug":"gpt-test"}));
        let official = resolve_target(&state, &json!({"model":"gpt-test"}))
            .await
            .expect("official");
        validate_external_max_output_tokens(
            &json!({"model":"gpt-test","max_output_tokens":"unchanged"}),
            &official,
        )
        .expect("official request is not interpreted or clamped");
    }

    #[test]
    fn external_response_exposes_picker_model_instead_of_upstream_model() {
        let normalized = expose_public_model(
            json!({"id":"resp_test","object":"response","model":"upstream-a"}),
            "external-a",
        )
        .expect("canonical external response");
        assert_eq!(normalized["model"], "external-a");
    }

    #[tokio::test]
    async fn model_max_output_override_wins_over_larger_preset_limit() {
        let target = zhipu_target(Some(4_096)).await;
        assert_eq!(target.max_output_tokens, Some(4_096));
        validate_external_max_output_tokens(
            &json!({"model":"glm-test","max_output_tokens":4_096}),
            &target,
        )
        .expect("model override limit is inclusive");
        let error = validate_external_max_output_tokens(
            &json!({"model":"glm-test","max_output_tokens":4_097}),
            &target,
        )
        .expect_err("model override must cap the preset");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn compact_response_id_is_replay_only_for_official_and_external_continuations() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let item = json!({
            "type":"compaction",
            "encrypted_content":"opaque-compact-fixture"
        });
        let owner = test_owner("official", "official-generation-a");
        let created_at = Utc::now();
        let mapping = CompactionRecord {
            response_id: compaction_key(&item).expect("compaction key"),
            source_provider: "official".into(),
            source_owner_id: Some(owner.clone()),
            portable_summary: "portable compact fixture".into(),
            encrypted_item: item.clone(),
            created_at,
        };
        // Mock an official compact upstream that omitted its response id.
        let normalized = normalize_compaction_response(&json!({}), "gpt-test", &item);
        let compact_id = normalized["id"]
            .as_str()
            .expect("local compact id")
            .to_owned();
        assert!(compact_id.starts_with(COMPACT_RESPONSE_ID_PREFIX));
        state
            .sessions
            .record_response_with_compactions(
                &ResponseRecord {
                    id: compact_id.clone(),
                    session_id: "session-compact".into(),
                    previous_response_id: None,
                    provider_id: "official".into(),
                    provider_owner_id: Some(owner.clone()),
                    model_id: "gpt-test".into(),
                    input: vec![json!({
                        "type":"message","role":"user","content":"history replaced"
                    })],
                    output: vec![item.clone()],
                    status: ResponseStatus::Completed,
                    incomplete_details: None,
                    created_at,
                },
                &[mapping],
            )
            .expect("record compact boundary");

        let user = json!({"type":"message","role":"user","content":"continue"});
        let mut official = json!({
            "model":"gpt-test",
            "previous_response_id":compact_id,
            "input":[user.clone()]
        });
        prepare_replay_for_owner(&state, &mut official, "official", Some(&owner))
            .expect("official replay");
        assert!(official.get("previous_response_id").is_none());
        assert_eq!(
            official["input"].as_array().expect("official input"),
            &[item.clone(), user.clone()]
        );

        let mut external = json!({
            "model":"glm-test",
            "previous_response_id":compact_id,
            "input":[user.clone()]
        });
        prepare_replay(&state, &mut external, "zhipu-test").expect("external replay");
        assert!(external.get("previous_response_id").is_none());
        let external_input = external["input"].as_array().expect("external input");
        assert_eq!(external_input.len(), 2);
        assert_eq!(external_input[0]["type"], "message");
        assert_eq!(external_input[0]["role"], "developer");
        assert_eq!(
            external_input[0].pointer("/metadata/cmr_portable_compaction"),
            Some(&Value::Bool(true))
        );
        assert_eq!(external_input[1], user);
    }

    #[test]
    fn external_current_input_cannot_persist_forged_official_provenance() {
        const SENTINEL: &str = "forged-official-reasoning-sentinel";
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let raw = vec![
            json!({
                "type":"reasoning",
                "encrypted_content":SENTINEL,
                "provider_metadata":{"source_provider_id":"official"}
            }),
            json!({
                "type":"message","role":"user","content":"keep message",
                "provider_metadata":{"source_provider_id":"official","secret":SENTINEL}
            }),
            json!({
                "type":"function_call","id":"fc_keep","call_id":"call_keep",
                "name":"tool","arguments":"{}"
            }),
            json!({
                "type":"function_call_output","call_id":"call_keep","output":"keep result"
            }),
        ];
        let stored = canonical_current_input_for_storage(&state, raw, "zhipu-test")
            .expect("canonical storage input");
        assert_eq!(stored.len(), 3);
        assert!(
            stored
                .iter()
                .all(|item| item.get("provider_metadata").is_none())
        );
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "resp_forged_provenance".into(),
                session_id: "session-provenance".into(),
                previous_response_id: None,
                provider_id: "zhipu-test".into(),
                provider_owner_id: None,
                model_id: "glm-test".into(),
                input: stored,
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("record external turn");
        let replay = state
            .sessions
            .replay_items("resp_forged_provenance", "official")
            .expect("replay official");
        let replay_json = serde_json::to_string(&replay).expect("serialize replay");
        assert!(!replay_json.contains(SENTINEL));
        assert!(!replay_json.contains("provider_metadata"));
        for kind in ["message", "function_call", "function_call_output"] {
            assert!(
                replay
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some(kind)),
                "missing preserved {kind}"
            );
        }
    }

    #[test]
    // This single table-driven test intentionally enumerates the complete
    // request-header allowlist for both response and catalog forwarding.
    #[allow(clippy::too_many_lines)]
    fn official_headers_use_an_explicit_protocol_allowlist() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer official-sentinel"),
        );
        headers.insert(header::COOKIE, HeaderValue::from_static("session=official"));
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("acct"),
        );
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("responses=experimental"),
        );
        headers.insert(
            HeaderName::from_static("openai-version"),
            HeaderValue::from_static("2026-01-01"),
        );
        headers.insert(
            HeaderName::from_static("openai-organization"),
            HeaderValue::from_static("org-sentinel"),
        );
        headers.insert(
            HeaderName::from_static("openai-project"),
            HeaderValue::from_static("project-sentinel"),
        );
        headers.insert(
            HeaderName::from_static("x-openai-client-version"),
            HeaderValue::from_static("1"),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::USER_AGENT, HeaderValue::from_static("cmr-test"));
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("third-party-sentinel"),
        );
        headers.insert(
            HeaderName::from_static("x-goog-api-key"),
            HeaderValue::from_static("google-sentinel"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("anthropic-sentinel"),
        );
        headers.insert(
            HeaderName::from_static("x-anthropic-api-key"),
            HeaderValue::from_static("anthropic-key-sentinel"),
        );
        headers.insert(
            HeaderName::from_static("x-custom-secret"),
            HeaderValue::from_static("custom-sentinel"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("localhost"));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("123"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );

        let result = forward_headers(&headers, true, false);
        for allowed in [
            "authorization",
            "cookie",
            "chatgpt-account-id",
            "openai-beta",
            "openai-version",
            "openai-organization",
            "openai-project",
            "x-openai-client-version",
            "accept",
            "content-type",
            "user-agent",
        ] {
            assert!(
                result.contains_key(allowed),
                "missing allowed header {allowed}"
            );
        }
        for forbidden in [
            "x-api-key",
            "x-goog-api-key",
            "anthropic-version",
            "x-anthropic-api-key",
            "x-custom-secret",
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
        ] {
            assert!(
                !result.contains_key(forbidden),
                "forwarded forbidden header {forbidden}"
            );
        }

        let catalog = forward_headers(&headers, true, true);
        assert!(!catalog.contains_key(header::ACCEPT));
        assert!(catalog.contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn external_headers_remain_isolated_from_official_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer official-sentinel"),
        );
        headers.insert(header::COOKIE, HeaderValue::from_static("session=official"));
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("acct"),
        );
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("responses=experimental"),
        );
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("inbound-third-party-sentinel"),
        );
        headers.insert(
            HeaderName::from_static("x-custom-secret"),
            HeaderValue::from_static("custom-sentinel"),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(header::USER_AGENT, HeaderValue::from_static("cmr-test"));

        let result = forward_headers(&headers, false, false);
        assert!(!result.contains_key(header::AUTHORIZATION));
        assert!(!result.contains_key(header::COOKIE));
        assert!(!result.contains_key("chatgpt-account-id"));
        assert!(!result.contains_key("openai-beta"));
        assert!(!result.contains_key("x-api-key"));
        assert!(!result.contains_key("x-custom-secret"));
        assert!(result.contains_key(header::ACCEPT));
        assert!(result.contains_key(header::USER_AGENT));
    }

    #[test]
    fn compaction_trigger_must_be_the_single_final_control_item() {
        let mut request = json!({
            "model": "glm-5.2",
            "input": [
                {"type":"message","role":"user","content":"continue"},
                {"type":"compaction_trigger"}
            ]
        });
        assert!(take_compaction_trigger(&mut request).unwrap());
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");

        let mut invalid = json!({
            "model": "glm-5.2",
            "input": [
                {"type":"compaction_trigger"},
                {"type":"message","role":"user","content":"after"}
            ]
        });
        assert!(take_compaction_trigger(&mut invalid).is_err());
    }

    #[test]
    fn compaction_rejects_pending_calls_and_accepts_both_call_id_forms() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "resp_pending_tools".into(),
                session_id: "session".into(),
                previous_response_id: None,
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-test".into(),
                input: Vec::new(),
                output: vec![
                    json!({
                        "type":"function_call",
                        "call_id":"call_by_call_id",
                        "name":"first",
                        "arguments":"{}"
                    }),
                    json!({
                        "type":"function_call",
                        "id":"call_by_item_id",
                        "name":"second",
                        "arguments":"{}"
                    }),
                ],
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("record response");

        let only_first_result = vec![json!({
            "type":"function_call_output",
            "call_id":"call_by_call_id",
            "output":"first result"
        })];
        let error =
            ensure_compaction_tools_closed(&state, Some("resp_pending_tools"), &only_first_result)
                .expect_err("second call remains pending");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.code, "invalid_request");
        assert!(error.message.contains("1 function call"));

        let all_results = vec![
            only_first_result[0].clone(),
            json!({
                "type":"function_call_output",
                "call_id":"call_by_item_id",
                "output":"second result"
            }),
        ];
        ensure_compaction_tools_closed(&state, Some("resp_pending_tools"), &all_results)
            .expect("all calls are paired");
    }

    #[test]
    fn completed_stream_output_must_exactly_match_done_items() {
        let mut response_id = "synthetic".to_owned();
        let stale_item = json!({"type":"message","id":"stale"});
        let canonical = json!({
            "type":"function_call",
            "call_id":"call_canonical",
            "name":"tool",
            "arguments":"{}"
        });
        let mut tap_state = StreamTapState::default();
        tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":stale_item
            }),
            &mut response_id,
            &mut tap_state,
        )
        .expect("stale message done");
        tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":canonical.clone()
            }),
            &mut response_id,
            &mut tap_state,
        )
        .expect("function call done");

        let error = tap_event(
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_canonical",
                    "status":"completed",
                    "output":[canonical.clone()]
                }
            }),
            &mut response_id,
            &mut tap_state,
        )
        .expect_err("terminal output cannot omit an already delivered done item");
        assert!(error.message.contains("exactly match"));
        assert!(tap_state.output.is_empty());
        assert!(tap_state.terminal_status.is_none());

        let mut valid_state = StreamTapState::default();
        tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":canonical.clone()
            }),
            &mut response_id,
            &mut valid_state,
        )
        .expect("canonical done item");
        tap_event(
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_canonical",
                    "status":"completed",
                    "output":[canonical.clone()]
                }
            }),
            &mut response_id,
            &mut valid_state,
        )
        .expect("valid exact completion");
        assert_eq!(response_id, "resp_canonical");
        assert_eq!(valid_state.output, vec![canonical]);

        let mut malformed_state = StreamTapState::default();
        let error = tap_event(
            &json!({
                "type":"response.completed",
                "response":{"id":"resp_malformed"}
            }),
            &mut response_id,
            &mut malformed_state,
        )
        .expect_err("completed output must be present");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.code, "upstream_error");
    }

    #[test]
    fn completed_stream_rejects_done_function_call_missing_from_terminal() {
        let streamed_call = json!({
            "type":"function_call",
            "call_id":"call_streamed_only",
            "name":"tool",
            "arguments":"{}"
        });
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":streamed_call.clone()
            }),
            &mut response_id,
            &mut state,
        )
        .expect("streamed call done");
        let error = tap_event(
            &json!({
                "type":"response.completed",
                "response":{"id":"resp_mismatch","output":[]}
            }),
            &mut response_id,
            &mut state,
        )
        .expect_err("terminal output omitted an already streamed call");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert!(state.output.is_empty());
        assert!(state.terminal_status.is_none());
    }

    #[test]
    fn completed_stream_rejects_terminal_function_call_without_done_item() {
        let terminal_call = json!({
            "type":"function_call",
            "id":"call_terminal_only",
            "name":"tool",
            "arguments":"{}"
        });
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        let error = tap_event(
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_mismatch",
                    "output":[terminal_call]
                }
            }),
            &mut response_id,
            &mut state,
        )
        .expect_err("terminal output introduced an unstreamed call");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert!(state.output.is_empty());
        assert!(state.terminal_status.is_none());
    }

    #[test]
    fn completed_stream_rejects_same_call_with_different_arguments() {
        let streamed = json!({
            "type":"function_call",
            "id":"fc_1",
            "call_id":"call_1",
            "name":"tool",
            "arguments":"{\"value\":1}"
        });
        let mut terminal = streamed.clone();
        terminal["arguments"] = Value::String("{\"value\":2}".into());
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":streamed
            }),
            &mut response_id,
            &mut state,
        )
        .expect("done item");
        let error = tap_event(
            &json!({
                "type":"response.completed",
                "response":{"id":"resp_mismatch","output":[terminal]}
            }),
            &mut response_id,
            &mut state,
        )
        .expect_err("terminal arguments must match the done item");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert!(state.terminal_status.is_none());
    }

    #[test]
    fn completed_stream_rejects_duplicate_terminal_function_call_id() {
        let call = json!({
            "type":"function_call",
            "id":"fc_1",
            "call_id":"call_duplicate",
            "name":"tool",
            "arguments":"{}"
        });
        let mut duplicate = call.clone();
        duplicate["id"] = Value::String("fc_2".into());
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        state.done_items.insert(0, call.clone());
        state.done_items.insert(1, duplicate.clone());
        let error = tap_event(
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_duplicate",
                    "status":"completed",
                    "output":[call, duplicate]
                }
            }),
            &mut response_id,
            &mut state,
        )
        .expect_err("duplicate terminal call_id must fail closed");
        assert!(error.message.contains("duplicate"));
        assert!(state.terminal_status.is_none());
    }

    #[test]
    fn completed_stream_uses_output_index_order_for_function_calls() {
        let first = json!({
            "type":"function_call","id":"fc_1","call_id":"call_1",
            "name":"first","arguments":"{}"
        });
        let second = json!({
            "type":"function_call","id":"fc_2","call_id":"call_2",
            "name":"second","arguments":"{}"
        });
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        for (index, item) in [(1, second.clone()), (0, first.clone())] {
            tap_event(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":index,
                    "item":item
                }),
                &mut response_id,
                &mut state,
            )
            .expect("out-of-order done item");
        }
        tap_event(
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_ordered",
                    "status":"completed",
                    "output":[first, second]
                }
            }),
            &mut response_id,
            &mut state,
        )
        .expect("output_index defines canonical order");
        assert_eq!(state.terminal_status, Some(ResponseStatus::Completed));
    }

    #[test]
    fn stream_rejects_events_after_completed_before_commit() {
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        tap_event(
            &json!({
                "type":"response.completed",
                "response":{"id":"resp_complete","status":"completed","output":[]}
            }),
            &mut response_id,
            &mut state,
        )
        .expect("terminal event is buffered");
        let error = tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"message","id":"late"}
            }),
            &mut response_id,
            &mut state,
        )
        .expect_err("late business event must abort before commit");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn stream_rejects_duplicate_done_index_and_item_id_before_completion() {
        let first = json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{"type":"message","id":"msg_1"}
        });
        let mut response_id = "synthetic".to_owned();
        let mut repeated_index = StreamTapState::default();
        tap_event(&first, &mut response_id, &mut repeated_index).expect("first done");
        let error = tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"message","id":"msg_2"}
            }),
            &mut response_id,
            &mut repeated_index,
        )
        .expect_err("duplicate output_index must fail");
        assert!(error.message.contains("output_index"));
        assert!(repeated_index.terminal_status.is_none());
        assert!(repeated_index.output.is_empty());

        let mut repeated_id = StreamTapState::default();
        tap_event(&first, &mut response_id, &mut repeated_id).expect("first done");
        let error = tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{"type":"message","id":"msg_1"}
            }),
            &mut response_id,
            &mut repeated_id,
        )
        .expect_err("duplicate item id must fail");
        assert!(error.message.contains("item id"));
        assert!(repeated_id.terminal_status.is_none());
        assert!(repeated_id.output.is_empty());
    }

    #[test]
    fn stream_rejects_duplicate_function_call_identity_on_second_done() {
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"function_call",
                    "id":"fc_1",
                    "call_id":"call_duplicate",
                    "name":"tool",
                    "arguments":"{}"
                }
            }),
            &mut response_id,
            &mut state,
        )
        .expect("first function call done");

        let error = tap_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{
                    "type":"function_call",
                    "id":"fc_2",
                    "call_id":"call_duplicate",
                    "name":"tool",
                    "arguments":"{}"
                }
            }),
            &mut response_id,
            &mut state,
        )
        .expect_err("the duplicate call identity must fail before forwarding");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert!(error.message.contains("duplicate"));
        assert_eq!(state.done_items.len(), 1);
        assert!(!state.done_items.contains_key(&1));
    }

    #[test]
    fn stream_rejects_non_executable_function_call_done_items() {
        for item in [
            json!({
                "type":"function_call",
                "id":"fc_missing_name",
                "arguments":"{}"
            }),
            json!({
                "type":"function_call",
                "id":"fc_non_string_arguments",
                "name":"tool",
                "arguments":{}
            }),
        ] {
            let mut response_id = "synthetic".to_owned();
            let mut state = StreamTapState::default();
            let error = tap_event(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":item
                }),
                &mut response_id,
                &mut state,
            )
            .expect_err("malformed function call must fail at done");
            assert_eq!(error.status, StatusCode::BAD_GATEWAY);
            assert!(state.done_items.is_empty());
        }
    }

    #[test]
    fn completed_batch_requests_immediate_stream_termination() {
        let mut response_id = "synthetic".to_owned();
        let mut state = StreamTapState::default();
        assert!(!stream_batch_completed(&state));
        tap_event(
            &json!({
                "type":"response.completed",
                "response":{"id":"resp_complete","status":"completed","output":[]}
            }),
            &mut response_id,
            &mut state,
        )
        .expect("valid terminal event");
        assert!(stream_batch_completed(&state));
    }

    #[test]
    fn malformed_sse_json_fails_closed() {
        assert!(parse_sse_data("").expect("empty event").is_none());
        assert!(parse_sse_data("[DONE]").expect("done event").is_none());
        assert_eq!(
            parse_sse_data(r#"{"type":"response.created"}"#)
                .expect("valid event")
                .expect("json value")["type"],
            "response.created"
        );

        let error = parse_sse_data("{not-json").expect_err("malformed data must fail");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.code, "upstream_error");
    }

    #[tokio::test]
    async fn external_history_requires_bound_account_even_for_official_target() {
        const PLAINTEXT_SENTINEL: &str = "account-a-plaintext-sentinel";

        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let account_a_digest: [u8; 32] = Sha256::digest(b"account-a").into();
        assert!(
            state
                .sessions
                .bind_or_verify_chatgpt_account(&account_a_digest)
                .expect("bind account A")
        );
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "resp_account_a".into(),
                session_id: "session-a".into(),
                previous_response_id: None,
                provider_id: "external-a".into(),
                provider_owner_id: None,
                model_id: "glm-a".into(),
                input: vec![json!({
                    "type":"message",
                    "role":"user",
                    "content":PLAINTEXT_SENTINEL
                })],
                output: vec![json!({
                    "type":"message",
                    "role":"assistant",
                    "content":[{"type":"output_text","text":"answer"}]
                })],
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("record account A response");

        let mut first_account_headers = HeaderMap::new();
        first_account_headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("account-a"),
        );
        first_account_headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer fixture-a"),
        );
        let mut second_account_headers = HeaderMap::new();
        second_account_headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("account-b"),
        );
        second_account_headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer fixture-b"),
        );
        state
            .remember_authenticated_client(&first_account_headers)
            .await;
        state
            .remember_authenticated_client(&second_account_headers)
            .await;

        let request = json!({
            "model":"gpt-test",
            "previous_response_id":"resp_account_a",
            "input":[{"type":"message","role":"user","content":"continue"}]
        });
        let rejected_request = request.clone();
        let error = require_request_account_scope(
            &state,
            &second_account_headers,
            &rejected_request,
            false,
            None,
        )
        .await
        .expect_err("account B must not resolve account A history");
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        assert!(!rejected_request.to_string().contains(PLAINTEXT_SENTINEL));

        require_request_account_scope(&state, &first_account_headers, &request, false, None)
            .await
            .expect("account A may continue its local history");
        let mut replayed_request = request;
        prepare_replay(&state, &mut replayed_request, "official").expect("prepare replay");
        assert!(replayed_request.to_string().contains(PLAINTEXT_SENTINEL));
        assert!(replayed_request.get("previous_response_id").is_none());
    }

    #[tokio::test]
    async fn unbound_native_official_continuation_skips_account_gate() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "resp_native_r1".into(),
                session_id: "session-native".into(),
                previous_response_id: None,
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-test".into(),
                input: vec![json!({"type":"message","role":"user","content":"r1"})],
                output: vec![json!({"type":"message","role":"assistant","content":[]})],
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("record native r1");

        let mut request = json!({
            "model":"gpt-test",
            "previous_response_id":"resp_native_r1",
            "input":[{"type":"message","role":"user","content":"r2"}]
        });
        require_request_account_scope(&state, &HeaderMap::new(), &request, false, None)
            .await
            .expect("native official r1 to r2 needs no local binding");
        prepare_replay(&state, &mut request, "official").expect("preserve native cursor");
        assert_eq!(request["previous_response_id"], "resp_native_r1");
        assert_eq!(request["input"].as_array().expect("input").len(), 1);
    }

    #[tokio::test]
    async fn native_official_continuations_are_independent_between_accounts() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let account_a_digest: [u8; 32] = Sha256::digest(b"account-a").into();
        assert!(
            state
                .sessions
                .bind_or_verify_chatgpt_account(&account_a_digest)
                .expect("bind account A")
        );
        for (id, session) in [
            ("resp_native_account_a", "session-a"),
            ("resp_native_account_b", "session-b"),
        ] {
            state
                .sessions
                .record_response(&ResponseRecord {
                    id: id.into(),
                    session_id: session.into(),
                    previous_response_id: None,
                    provider_id: "official".into(),
                    provider_owner_id: None,
                    model_id: "gpt-test".into(),
                    input: Vec::new(),
                    output: Vec::new(),
                    status: ResponseStatus::Completed,
                    incomplete_details: None,
                    created_at: Utc::now(),
                })
                .expect("record native response");
        }

        for (account, previous) in [
            ("account-a", "resp_native_account_a"),
            ("account-b", "resp_native_account_b"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_str(account).expect("account header"),
            );
            let mut request = json!({
                "model":"gpt-test",
                "previous_response_id":previous,
                "input":[]
            });
            require_request_account_scope(&state, &headers, &request, false, None)
                .await
                .expect("native official continuation bypasses local account binding");
            prepare_replay(&state, &mut request, "official").expect("preserve native cursor");
            assert_eq!(request["previous_response_id"], previous);
        }
    }

    #[tokio::test]
    async fn warmup_and_external_requests_still_require_account_binding() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let warmup_id = format!("{WARMUP_RESPONSE_ID_PREFIX}account_gate");
        state
            .sessions
            .record_response(&ResponseRecord {
                id: warmup_id.clone(),
                session_id: "session-warmup".into(),
                previous_response_id: Some("resp_native_root".into()),
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-test".into(),
                input: vec![json!({"type":"message","role":"user","content":"warmup"})],
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("record warmup");

        let warmup_request = json!({
            "model":"gpt-test",
            "previous_response_id":warmup_id,
            "input":[]
        });
        let warmup_error =
            require_request_account_scope(&state, &HeaderMap::new(), &warmup_request, false, None)
                .await
                .expect_err("synthetic warmup replay must be account-bound");
        assert_eq!(warmup_error.status, StatusCode::UNAUTHORIZED);

        let compact_request = json!({
            "model":"gpt-test",
            "previous_response_id":format!("{COMPACT_RESPONSE_ID_PREFIX}account_gate"),
            "input":[]
        });
        let compact_error =
            require_request_account_scope(&state, &HeaderMap::new(), &compact_request, false, None)
                .await
                .expect_err("replay-only compact cursor must be account-bound");
        assert_eq!(compact_error.status, StatusCode::UNAUTHORIZED);

        let external_request = json!({"model":"glm-test","input":[]});
        let external_error =
            require_request_account_scope(&state, &HeaderMap::new(), &external_request, true, None)
                .await
                .expect_err("every external request must be account-bound");
        assert_eq!(external_error.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn compaction_lifecycle_contains_exactly_one_standard_item() {
        let encrypted = json!({
            "output": [{"type":"compaction","encrypted_content":"opaque"}]
        });
        let item = validate_compaction_response(&encrypted).unwrap();
        let response = normalize_compaction_response(&encrypted, "glm-5.2", item);
        let events = compaction_events(&response);
        assert_eq!(events.len(), 5);
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[4]["type"], "response.completed");
        let output = events[4]["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "compaction");
        assert!(
            events
                .iter()
                .all(|event| event.get("sequence_number").is_some())
        );
    }

    #[test]
    fn stream_errors_use_the_responses_top_level_shape() {
        let event = response_error_event("adapter_error", "bad chunk", 7);
        assert_eq!(event["type"], "error");
        assert_eq!(event["code"], "adapter_error");
        assert_eq!(event["message"], "bad chunk");
        assert_eq!(event["sequence_number"], 7);
        assert!(event.get("error").is_none());
    }

    #[test]
    fn portable_compaction_summary_is_control_context_not_assistant_speech() {
        let item = portable_compaction_summary("portable state");
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "developer");
        assert_eq!(item["content"][0]["type"], "input_text");
        assert_eq!(item["content"][0]["text"], "portable state");
        assert_ne!(item["role"], "assistant");
        assert_ne!(item["type"], "compaction");
    }

    #[test]
    fn local_websocket_warmup_ids_are_distinguishable_from_upstream_ids() {
        let id = format!("{WARMUP_RESPONSE_ID_PREFIX}{}", Uuid::nil());
        assert!(id.starts_with(WARMUP_RESPONSE_ID_PREFIX));
        assert!(!"resp_upstream_native".starts_with(WARMUP_RESPONSE_ID_PREFIX));
    }

    #[test]
    fn every_standard_terminal_stream_event_is_classified_once() {
        assert_eq!(
            stream_terminal(&json!({"type":"response.completed"})),
            Some(StreamTerminal::Completed)
        );
        assert_eq!(
            stream_terminal(&json!({"type":"response.incomplete"})),
            Some(StreamTerminal::Incomplete)
        );
        for kind in ["response.failed", "error"] {
            assert_eq!(
                stream_terminal(&json!({"type":kind})),
                Some(StreamTerminal::Failed)
            );
        }
        assert_eq!(
            stream_terminal(&json!({"type":"response.output_text.delta"})),
            None
        );
    }

    #[test]
    fn provider_instance_id_owns_private_reasoning_provenance() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let first_owner = test_owner("account-a", "credential-generation-a");
        let rotated_owner = test_owner("account-a", "credential-generation-b");
        assert_ne!(first_owner, rotated_owner);

        let direct = vec![
            json!({
                "type":"reasoning",
                "provider_metadata":{
                    "cmr_provider_owner_id":first_owner.as_str(),
                    "source_provider_id":"account-a",
                    "format":"openai_chat.reasoning_content.v1",
                    "payload":{"reasoning_content":"foreign-private-sentinel"}
                }
            }),
            json!({
                "type":"message",
                "role":"user",
                "content":"portable",
                "provider_metadata":{
                    "cmr_provider_owner_id":first_owner.as_str(),
                    "source_provider_id":"account-a",
                    "format":"private-message.v1",
                    "payload":{"secret":"foreign-private-sentinel"}
                }
            }),
            json!({
                "type":"reasoning",
                "provider_metadata":{
                    "source_provider_id":first_owner.as_str(),
                    "payload":{"reasoning_content":"legacy-owner-sentinel"}
                }
            }),
            json!({
                "type":"reasoning",
                "encrypted_content":"untrusted-direct-official-sentinel"
            }),
        ];
        let same =
            portable_input_items_for_owner(&state, direct.clone(), "account-a", Some(&first_owner))
                .expect("same exact owner");
        assert_eq!(same.len(), 2);
        assert!(
            same.iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        );
        let rotated =
            portable_input_items_for_owner(&state, direct, "account-a", Some(&rotated_owner))
                .expect("rotated credential owner");
        assert_eq!(rotated.len(), 1);
        assert!(
            rotated
                .iter()
                .all(|item| item.get("provider_metadata").is_none())
        );
        let rotated_json = serde_json::to_string(&rotated).expect("serialize rotated input");
        assert!(!rotated_json.contains("foreign-private-sentinel"));
        assert!(!rotated_json.contains("legacy-owner-sentinel"));
        assert!(!rotated_json.contains("untrusted-direct-official-sentinel"));
    }

    #[test]
    fn official_warmup_preserves_a_native_pre_router_cursor() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let warmup_id = format!("{WARMUP_RESPONSE_ID_PREFIX}fixture");
        state
            .sessions
            .record_response(&ResponseRecord {
                id: warmup_id.clone(),
                session_id: "session".into(),
                previous_response_id: Some("resp_native_before_router".into()),
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-test".into(),
                input: vec![json!({
                    "type":"message",
                    "role":"user",
                    "content":"warmup context"
                })],
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("record warmup");
        let mut request = json!({
            "model":"gpt-test",
            "previous_response_id":warmup_id,
            "input":[{"type":"message","role":"user","content":"current turn"}]
        });
        prepare_replay(&state, &mut request, "official").expect("prepare replay");
        assert_eq!(request["previous_response_id"], "resp_native_before_router");
        let input = request["input"].as_array().expect("input array");
        assert_eq!(input.len(), 2);
        assert!(
            input
                .iter()
                .any(|item| item.to_string().contains("warmup context"))
        );
        assert!(
            input
                .iter()
                .any(|item| item.to_string().contains("current turn"))
        );
    }

    #[test]
    fn compact_then_warmup_replays_without_a_synthetic_official_cursor() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let compaction = json!({
            "type":"compaction",
            "encrypted_content":"opaque-compact-warmup-fixture"
        });
        let owner = test_owner("official", "official-generation-warmup");
        let created_at = Utc::now();
        let mapping = CompactionRecord {
            response_id: compaction_key(&compaction).expect("compaction key"),
            source_provider: "official".into(),
            source_owner_id: Some(owner.clone()),
            portable_summary: "portable compact warmup fixture".into(),
            encrypted_item: compaction.clone(),
            created_at,
        };
        let compact_id = format!("{COMPACT_RESPONSE_ID_PREFIX}warmup_predecessor");
        state
            .sessions
            .record_response_with_compactions(
                &ResponseRecord {
                    id: compact_id.clone(),
                    session_id: "session-compact-warmup".into(),
                    previous_response_id: None,
                    provider_id: "official".into(),
                    provider_owner_id: Some(owner.clone()),
                    model_id: "gpt-test".into(),
                    input: vec![json!({
                        "type":"message","role":"user","content":"history replaced"
                    })],
                    output: vec![compaction.clone()],
                    status: ResponseStatus::Completed,
                    incomplete_details: None,
                    created_at,
                },
                &[mapping],
            )
            .expect("record compact boundary");
        let warmup = json!({
            "type":"message","role":"user","content":"warmup context"
        });
        let warmup_id = format!("{WARMUP_RESPONSE_ID_PREFIX}after_compact");
        state
            .sessions
            .record_response(&ResponseRecord {
                id: warmup_id.clone(),
                session_id: "session-compact-warmup".into(),
                previous_response_id: Some(compact_id),
                provider_id: "official".into(),
                provider_owner_id: Some(owner.clone()),
                model_id: "gpt-test".into(),
                input: vec![warmup.clone()],
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("record warmup");
        let current = json!({
            "type":"message","role":"user","content":"current turn"
        });
        let mut request = json!({
            "model":"gpt-test",
            "previous_response_id":warmup_id,
            "input":[current.clone()]
        });

        prepare_replay_for_owner(&state, &mut request, "official", Some(&owner))
            .expect("prepare replay");

        assert!(request.get("previous_response_id").is_none());
        let input = request["input"].as_array().expect("input array");
        assert_eq!(input, &[compaction, warmup, current]);
        assert_eq!(
            input
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
                .count(),
            1
        );
    }

    #[test]
    fn external_turns_reinject_cached_harness_tools() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        // The client sends tool definitions once in a warmup frame; later
        // turns rely on Responses server-side state the router must restore.
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "cmr_warmup_seed".into(),
                session_id: "session-seed".into(),
                previous_response_id: None,
                provider_id: "zhipu".into(),
                provider_owner_id: None,
                model_id: "glm-5.3".into(),
                input: vec![json!({
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {"type":"custom","name":"exec","description":"Run JavaScript code"},
                        {"type":"function","name":"wait","description":"Wait","parameters":{"type":"object"}}
                    ],
                })],
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("seed warmup record");

        let zhipu = preset_by_id("zhipu").expect("zhipu preset");
        let target = Target {
            provider_id: "zhipu".into(),
            public_model: "glm-5.3".into(),
            upstream_model: "glm-5.3".into(),
            preset: zhipu.clone(),
            max_output_tokens: None,
            external: true,
            secret_ref: None,
            provider_owner_id: None,
        };

        let mut request = json!({
            "model": "glm-5.3",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"继续"}]}
            ]
        });
        ensure_external_harness_tools(&state, &mut request, &target).expect("inject tools");
        let items = request["input"].as_array().expect("input array");
        assert_eq!(items[0]["type"], "additional_tools");
        assert_eq!(items[0]["tools"][0]["name"], "exec");
        assert_eq!(items.len(), 2, "original items stay after the injection");

        // A turn that already carries definitions must not be duplicated.
        ensure_external_harness_tools(&state, &mut request, &target).expect("idempotent check");
        let definitions = request["input"]
            .as_array()
            .expect("input array")
            .iter()
            .filter(|item| item["type"] == "additional_tools")
            .count();
        assert_eq!(definitions, 1);
    }

    #[test]
    fn external_turns_fall_back_to_builtin_harness_tools() {
        // No stored warmup carries callable tool definitions, so the router
        // must supply the official code-mode definitions itself.
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "cmr_warmup_namespace".into(),
                session_id: "session-ns".into(),
                previous_response_id: None,
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-5.6-luna".into(),
                input: vec![json!({
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{"type": "namespace", "name": "functions"}],
                })],
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("seed namespace-only warmup");

        let zhipu = preset_by_id("zhipu").expect("zhipu preset");
        let target = Target {
            provider_id: "zhipu".into(),
            public_model: "glm-5.3".into(),
            upstream_model: "glm-5.3".into(),
            preset: zhipu.clone(),
            max_output_tokens: None,
            external: true,
            secret_ref: None,
            provider_owner_id: None,
        };
        let mut request = json!({
            "model": "glm-5.3",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"继续"}]}
            ]
        });
        ensure_external_harness_tools(&state, &mut request, &target).expect("inject builtin tools");
        let items = request["input"].as_array().expect("input array");
        assert_eq!(items[0]["type"], "additional_tools");
        let names: Vec<&str> = items[0]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"exec"), "builtin exec missing: {names:?}");
        assert!(names.contains(&"wait"), "builtin wait missing: {names:?}");
        assert!(
            !names.contains(&"functions"),
            "namespace declarations must not be injected: {names:?}"
        );
    }

    #[test]
    fn external_switch_never_replays_across_a_compaction_boundary_even_when_window_fits() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "zhipu".into(),
            preset: "zhipu".into(),
            base_url: None,
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "glm-5.3".into(),
            display_name: "glm-5.3".into(),
            provider: "zhipu".into(),
            upstream_model: "glm-5.3".into(),
            order: 0,
            enabled: true,
            context_window: Some(1_000_000),
            max_output_tokens: None,
        });
        let state = AppState::with_credentials(
            config,
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        // Recorded pre-compaction turn followed by a response whose input
        // carries an official compaction item without a portable mapping.
        let seed = |id: &str, previous: Option<&str>, user: &str, assistant: &str| {
            state
                .sessions
                .record_response(&ResponseRecord {
                    id: id.into(),
                    session_id: "session-lossless".into(),
                    previous_response_id: previous.map(str::to_owned),
                    provider_id: "official".into(),
                    provider_owner_id: None,
                    model_id: "gpt-5.6-luna".into(),
                    input: vec![json!({
                        "type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": user}]
                    })],
                    output: vec![json!({
                        "type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": assistant}]
                    })],
                    status: ResponseStatus::Completed,
                    incomplete_details: None,
                    created_at: Utc::now(),
                })
                .expect("seed record");
        };
        seed(
            "resp_pre_compaction",
            None,
            "pre-compaction turn",
            "early answer",
        );
        seed(
            "resp_post_compaction",
            Some("resp_pre_compaction"),
            "post-compaction turn",
            "later answer",
        );

        let mut request = json!({
            "model": "glm-5.3",
            "previous_response_id": "resp_post_compaction",
            "input": [
                {"type": "compaction", "encrypted_content": "official-only"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"继续"}]}
            ]
        });
        prepare_replay_for_owner(&state, &mut request, "zhipu", None).expect("boundary replay");
        let replay = request["input"].as_array().expect("replay array");
        let text: Vec<&str> = replay
            .iter()
            .filter_map(|item| item.pointer("/content/0/text").and_then(Value::as_str))
            .collect();
        // A compaction is a deliberate forgetting operation: even with a huge
        // external window the pre-compaction turns must not be resurrected.
        for forgotten in [
            "pre-compaction turn",
            "early answer",
            "post-compaction turn",
        ] {
            assert!(
                !text.contains(&forgotten),
                "no replay of {forgotten}: {text:?}"
            );
        }
        assert!(
            text.iter().any(|t| t.contains("context note")),
            "unmapped official compaction degrades to the neutral note: {text:?}"
        );
        assert!(
            text.contains(&"继续"),
            "the current turn is preserved: {text:?}"
        );
        assert!(
            !replay.iter().any(|item| item["type"] == "compaction"),
            "compaction items must not be replayed: {replay:?}"
        );
        assert!(request.get("previous_response_id").is_none());
    }

    #[test]
    fn external_switch_falls_back_to_compaction_boundary_when_window_is_small() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "zhipu".into(),
            preset: "zhipu".into(),
            base_url: None,
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.push(ModelConfig {
            id: "glm-5.3".into(),
            display_name: "glm-5.3".into(),
            provider: "zhipu".into(),
            upstream_model: "glm-5.3".into(),
            order: 0,
            enabled: true,
            context_window: Some(1_000),
            max_output_tokens: None,
        });
        let state = AppState::with_credentials(
            config,
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        let bulky_history = "recorded pre-compaction context ".repeat(200);
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "resp_pre_small".into(),
                session_id: "session-small".into(),
                previous_response_id: None,
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-5.6-luna".into(),
                input: vec![json!({
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": bulky_history}]
                })],
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("seed pre-compaction record");
        state
            .sessions
            .record_response(&ResponseRecord {
                id: "resp_post_small".into(),
                session_id: "session-small".into(),
                previous_response_id: Some("resp_pre_small".into()),
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-5.6-luna".into(),
                input: vec![json!({
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "recent turn"}]
                })],
                output: Vec::new(),
                status: ResponseStatus::Completed,
                incomplete_details: None,
                created_at: Utc::now(),
            })
            .expect("seed post-compaction record");

        let mut request = json!({
            "model": "glm-5.3",
            "previous_response_id": "resp_post_small",
            "input": [
                {"type": "compaction", "encrypted_content": "official-only"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"继续"}]}
            ]
        });
        prepare_replay_for_owner(&state, &mut request, "zhipu", None).expect("boundary replay");
        let text: Vec<&str> = request["input"]
            .as_array()
            .expect("replay array")
            .iter()
            .filter_map(|item| item.pointer("/content/0/text").and_then(Value::as_str))
            .collect();
        assert!(
            text.iter().any(|t| t.contains("context note")),
            "small window keeps the neutral note: {text:?}"
        );
        assert!(
            !text
                .iter()
                .any(|t| t.contains("recorded pre-compaction context")),
            "overflowing pre-compaction history is cut: {text:?}"
        );
    }

    #[test]
    fn unmapped_official_compaction_degrades_to_a_neutral_note_for_external() {
        let state = AppState::with_credentials(
            RouterConfig::default(),
            StateStore::in_memory().expect("state"),
            Arc::new(MemoryCredentialStore::default()),
        )
        .expect("app state");
        // An official-side compaction the router never compacted itself: no
        // CompactionRecord exists, so its encrypted payload cannot be replayed
        // verbatim to an external provider.
        let opaque = json!({
            "type":"compaction",
            "encrypted_content":"official-only-opaque-history"
        });
        let after = json!({
            "type":"message","role":"user","content":"recent turn after compaction"
        });
        let portable = portable_input_items(
            &state,
            vec![
                json!({"type":"message","role":"user","content":"pre-compaction history"}),
                opaque,
                after.clone(),
            ],
            "zhipu",
        )
        .expect("provider switch continues instead of failing");

        // Everything before the compaction boundary is gone, the opaque item is
        // replaced by a neutral note, and post-compaction items still replay.
        assert_eq!(portable.len(), 2);
        assert_ne!(
            portable[0].get("type").and_then(Value::as_str),
            Some("compaction")
        );
        let note = portable[0]
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .expect("neutral note text");
        assert!(note.contains("not available"));
        assert_eq!(portable[1], after);
    }
}
