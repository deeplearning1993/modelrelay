use std::{
    collections::HashSet,
    fmt::Write as _,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, OriginalUri, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use cmr_storage::{ConfigInstanceId, CredentialStore, OsCredentialStore, RouterConfig, StateStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use crate::{Result, RouterError, catalog, transport};

/// Persistence key for the official catalog query string last accepted from a
/// real client. The official backend rejects `/models` without `client_version`,
/// so credential re-verifications replay this query after service restarts.
const OFFICIAL_CATALOG_QUERY_KEY: &str = "official_catalog_query";

/// Persistence key for the ordered official model slugs of the last authorized
/// catalog. Restoring them at startup lets compaction pick the official model
/// before the first catalog fetch of a new process.
const OFFICIAL_CATALOG_MODELS_KEY: &str = "official_catalog_models";

/// Shared immutable configuration and synchronized runtime state.
#[derive(Clone)]
pub struct AppState {
    pub(crate) config: Arc<RouterConfig>,
    pub(crate) client: reqwest::Client,
    pub(crate) sessions: Arc<StateStore>,
    pub(crate) credentials: Arc<dyn CredentialStore>,
    pub(crate) config_instance_id: Option<ConfigInstanceId>,
    pub(crate) official_models: Arc<RwLock<Vec<Value>>>,
    /// External model IDs actually injected by the most recent authorized
    /// catalog merge. `None` means no authorized catalog has been cached yet;
    /// `Some(Vec::new())` means authorization succeeded but picker capacity
    /// admitted no external models.
    pub(crate) authorized_external_models: Arc<RwLock<Option<Vec<String>>>>,
    authorized_catalog_cache_lock: Arc<Mutex<()>>,
    /// SHA-256 fingerprints of credentials that the official catalog endpoint
    /// has accepted during this process. The credentials themselves are never
    /// retained, logged, or exposed to external providers.
    pub(crate) authenticated_clients: Arc<RwLock<HashSet<[u8; 32]>>>,
    /// Fingerprints admitted while the official backend was unreachable. The
    /// persistent account binding still gated them; the set is process-local
    /// so a restart re-verifies once the backend is reachable again.
    pub(crate) degraded_clients: Arc<RwLock<HashSet<[u8; 32]>>>,
    /// Query string of the most recent client catalog request. The official
    /// backend rejects `/models` without a `client_version` query parameter,
    /// so later credential re-verifications replay this exact query instead of
    /// issuing a parameterless request that can only ever fail.
    last_client_query: Arc<std::sync::Mutex<Option<String>>>,
}

impl AppState {
    /// Creates a runtime with the operating-system credential store.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the HTTP client
    /// cannot be constructed.
    pub fn new(config: RouterConfig, sessions: StateStore) -> Result<Self> {
        Self::with_credentials(config, sessions, Arc::new(OsCredentialStore))
    }

    /// Creates a production runtime whose vault and provider provenance are
    /// isolated to one exact configuration path.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the HTTP client
    /// cannot be constructed.
    pub fn new_scoped(
        config: RouterConfig,
        sessions: StateStore,
        instance_id: ConfigInstanceId,
    ) -> Result<Self> {
        let credentials = Arc::new(OsCredentialStore::scoped(instance_id.clone()));
        Self::with_credentials_and_instance(config, sessions, credentials, Some(instance_id))
    }

    /// Creates a runtime with an injected credential store, useful for tests.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the HTTP client
    /// cannot be constructed.
    pub fn with_credentials(
        config: RouterConfig,
        sessions: StateStore,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self> {
        Self::with_credentials_and_instance(config, sessions, credentials, None)
    }

    /// Creates a runtime with injected credentials and an exact config-instance
    /// provenance namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration validation, interrupted-response
    /// recovery, or HTTP client construction fails.
    pub fn with_credentials_and_instance(
        config: RouterConfig,
        sessions: StateStore,
        credentials: Arc<dyn CredentialStore>,
        config_instance_id: Option<ConfigInstanceId>,
    ) -> Result<Self> {
        config.validate()?;
        // Recover before the service can accept a request. Journaled output is
        // retained as a standard incomplete response for continuation, but is
        // never replayed as a fresh client-side stream event.
        sessions.recover_interrupted_responses()?;
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .timeout(std::time::Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        // Restore the last catalog query accepted by the official backend so
        // credential re-verification works before the first catalog fetch of
        // this process.
        let persisted_query = sessions
            .metadata_get(OFFICIAL_CATALOG_QUERY_KEY)
            .ok()
            .flatten();
        // Restore the official model slugs so compaction can pick the official
        // model before the first catalog fetch of this process.
        let persisted_official_models = sessions
            .metadata_get(OFFICIAL_CATALOG_MODELS_KEY)
            .ok()
            .flatten()
            .and_then(|encoded| {
                serde_json::from_str::<Vec<String>>(&encoded)
                    .ok()
                    .map(|slugs| {
                        slugs
                            .into_iter()
                            .map(|slug| json!({"slug": slug}))
                            .collect::<Vec<_>>()
                    })
            })
            .unwrap_or_default();
        Ok(Self {
            config: Arc::new(config),
            client,
            sessions: Arc::new(sessions),
            credentials,
            config_instance_id,
            official_models: Arc::new(RwLock::new(persisted_official_models)),
            authorized_external_models: Arc::new(RwLock::new(None)),
            authorized_catalog_cache_lock: Arc::new(Mutex::new(())),
            authenticated_clients: Arc::new(RwLock::new(HashSet::new())),
            degraded_clients: Arc::new(RwLock::new(HashSet::new())),
            last_client_query: Arc::new(std::sync::Mutex::new(persisted_query)),
        })
    }
}

/// Constructs the Axum service without opening a socket.
pub fn build_router(state: AppState) -> Router {
    let body_limit = state.config.server.max_body_bytes;
    Router::new()
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .route("/models", get(models))
        .route("/v1/models", get(models))
        .route("/responses", post(transport::responses).get(responses_ws))
        .route(
            "/v1/responses",
            post(transport::responses).get(responses_ws),
        )
        .route("/responses/compact", post(transport::compact))
        .route("/v1/responses/compact", post(transport::compact))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

/// Binds only the configured loopback IP and serves until shutdown.
///
/// # Errors
///
/// Returns an error for a non-loopback address, a bind failure, or an HTTP
/// server failure.
pub async fn serve(state: AppState) -> Result<()> {
    let ip: IpAddr = state
        .config
        .server
        .host
        .parse()
        .map_err(|_| RouterError::bad_request("server host is not a valid IP address"))?;
    if !ip.is_loopback() {
        return Err(RouterError::bad_request("refusing non-loopback listener"));
    }
    let address = SocketAddr::new(ip, state.config.server.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| RouterError::internal(format!("cannot bind {address}: {error}")))?;
    tracing::info!(%address, "ModelRelay listening");
    axum::serve(listener, build_router(state))
        .await
        .map_err(|error| RouterError::internal(format!("HTTP server stopped: {error}")))
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    let authorized_external_models = state.authorized_external_models.read().await.clone();
    let mut routable_external_models: Vec<_> = state
        .config
        .models
        .iter()
        .filter(|model| crate::catalog::is_published_external_model(&state.config, model))
        .map(|model| model.id.clone())
        .collect();
    routable_external_models.sort();
    routable_external_models.dedup();
    Json(json!({
        "status": "ok",
        "service": "codex-model-router",
        "version": env!("CARGO_PKG_VERSION"),
        "listen": format!("{}:{}", state.config.server.host, state.config.server.port),
        "external_models": authorized_external_models.clone().unwrap_or_default(),
        "routable_external_models": routable_external_models,
        "official_catalog_cached": authorized_external_models.is_some(),
    }))
}

async fn models(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Response> {
    reject_cross_origin(&headers)?;
    let mut catalog_url = format!(
        "{}/models",
        state.config.official_base_url.trim_end_matches('/')
    );
    if let Some(query) = uri.query() {
        catalog_url.push('?');
        catalog_url.push_str(query);
    }
    let upstream = state
        .client
        .get(catalog_url)
        .headers(transport::forward_headers(&headers, true, true))
        .send()
        .await?;
    let status = upstream.status();
    if !status.is_success() {
        return Ok(raw_response(
            status,
            "application/json",
            serde_json::to_vec(&json!({
                "error": {
                    "code": "official_catalog_error",
                    "message": "official ChatGPT backend rejected the model catalog request"
                }
            }))?
            .into(),
        ));
    }
    let official: Value = upstream.json().await?;
    let official_items = official_catalog_models(&official)?.to_vec();
    // The official backend just accepted this exact query; keep it so later
    // credential re-verifications do not send a parameterless `/models`.
    state.remember_client_query(uri.query());

    // The official subscription catalog remains available regardless of the
    // local external-model authorization state. Only a single, non-empty
    // account header that is unbound or matches the persisted binding may
    // enroll credentials and receive injected external entries.
    let eligible_digest = if let Some(digest) = catalog_chatgpt_account_digest(&headers) {
        if let Ok(binding_match) = state.sessions.chatgpt_account_matches(&digest) {
            catalog_account_is_eligible(Some(digest), binding_match)
        } else {
            tracing::warn!(
                "could not verify ChatGPT account binding; returning official catalog only"
            );
            None
        }
    } else {
        None
    };
    let authorized_catalog = if let Some(digest) = eligible_digest {
        // Validate collisions before binding the account. Once authorized, an
        // ambiguous model id must fail closed rather than displaying an
        // official entry that request routing would interpret as external.
        let merged = catalog::merge_catalog(&state.config, official.clone())?;
        match state.sessions.bind_or_verify_chatgpt_account(&digest) {
            Ok(true) => Some(merged),
            Ok(false) => None,
            Err(_) => {
                tracing::warn!(
                    "could not persist ChatGPT account binding; returning official catalog only"
                );
                None
            }
        }
    } else {
        None
    };
    let visible_catalog = if let Some(merged) = authorized_catalog {
        state
            .cache_authorized_catalog(official_items, &merged)
            .await;
        state.remember_authenticated_client(&headers).await;
        merged
    } else {
        official
    };
    let encoded = serde_json::to_vec(&visible_catalog).map_err(RouterError::from)?;
    let digest = Sha256::digest(&encoded);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let etag = format!("W/\"{hex}\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let mut response = raw_response(StatusCode::OK, "application/json", encoded.into());
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag)
            .map_err(|_| RouterError::internal("generated invalid ETag"))?,
    );
    Ok(response)
}

impl AppState {
    async fn cache_authorized_catalog(&self, official_items: Vec<Value>, merged: &Value) {
        let external_models = catalog::injected_external_model_ids(&self.config, merged);
        let _cache_guard = self.authorized_catalog_cache_lock.lock().await;
        // Publish the health snapshot only after the official routing cache is
        // ready. Concurrent health checks can briefly observe the previous
        // authorized snapshot, never a capacity-blind or cold-start claim.
        let official_slugs: Vec<String> = official_items
            .iter()
            .filter_map(|model| catalog::model_id(model).map(str::to_owned))
            .collect();
        if let Ok(encoded) = serde_json::to_string(&official_slugs) {
            let _ = self
                .sessions
                .metadata_set(OFFICIAL_CATALOG_MODELS_KEY, &encoded);
        }
        *self.official_models.write().await = official_items;
        *self.authorized_external_models.write().await = Some(external_models);
    }

    /// Records only a one-way fingerprint after the official backend has
    /// accepted these request credentials. A successful verification also
    /// removes any degraded-mode admission for the same fingerprint, so the
    /// client exits degraded mode as soon as the backend is reachable again.
    pub(crate) async fn remember_authenticated_client(&self, headers: &HeaderMap) {
        if let Some(fingerprint) = client_credential_fingerprint(headers) {
            self.degraded_clients.write().await.remove(&fingerprint);
            self.authenticated_clients.write().await.insert(fingerprint);
        }
    }

    /// Remembers the query string of a catalog request the official backend
    /// just accepted, so later credential re-verifications replay it verbatim.
    /// The value is persisted so re-verification also works immediately after
    /// a service restart, before the first catalog fetch of the new process.
    fn remember_client_query(&self, query: Option<&str>) {
        if let Some(query) = query {
            if let Ok(mut guard) = self.last_client_query.lock() {
                *guard = Some(query.to_owned());
            }
            let _ = self
                .sessions
                .metadata_set(OFFICIAL_CATALOG_QUERY_KEY, query);
        }
    }

    /// Best query string for an official `/models` re-verification: the
    /// triggering request's own query, falling back to the last query a real
    /// client used for a successful catalog fetch.
    fn verification_query(&self, request_query: Option<&str>) -> Option<String> {
        if request_query.is_some_and(|query| query.contains("client_version=")) {
            return request_query.map(str::to_owned);
        }
        self.last_client_query
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Prevents arbitrary local processes from spending configured external
    /// provider quota. The persistent account binding is checked before a
    /// process-local credential enrollment. Binding is performed only by the
    /// public model-catalog handler after it validates an official catalog.
    pub(crate) async fn require_authenticated_client(
        &self,
        headers: &HeaderMap,
        request_query: Option<&str>,
    ) -> Result<()> {
        let account_digest = required_chatgpt_account_digest(headers)?;
        match self.sessions.chatgpt_account_matches(&account_digest)? {
            Some(true) => {}
            Some(false) => return Err(account_binding_mismatch()),
            None => {
                return Err(RouterError::unauthorized(
                    "external models require a ChatGPT account bound through the model catalog",
                ));
            }
        }
        let fingerprint = client_credential_fingerprint(headers).ok_or_else(|| {
            RouterError::unauthorized(
                "external models require credentials accepted by the official ChatGPT backend",
            )
        })?;
        if self
            .authenticated_clients
            .read()
            .await
            .contains(&fingerprint)
            || self.degraded_clients.read().await.contains(&fingerprint)
        {
            return Ok(());
        }
        let mut url = format!(
            "{}/models",
            self.config.official_base_url.trim_end_matches('/')
        );
        // The official backend answers `/models` with HTTP 400 unless the
        // query carries `client_version`. Re-verify with the triggering
        // request's query, or the last one a real client used successfully.
        if let Some(query) = self.verification_query(request_query) {
            url.push('?');
            url.push_str(&query);
        }
        let response = self
            .client
            .get(url)
            .timeout(std::time::Duration::from_secs(10))
            .headers(transport::forward_headers(headers, true, true))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                // The persistent account binding already matched above. When
                // the official backend is merely unreachable (e.g. the user's
                // proxy dropped), local external models must keep working;
                // credentials the backend actively rejects still fail below
                // with an explicit HTTP response.
                tracing::warn!(
                    %error,
                    "official verification unreachable; admitting the bound account in degraded mode"
                );
                self.degraded_clients.write().await.insert(fingerprint);
                return Ok(());
            }
        };
        if !response.status().is_success() {
            return Err(RouterError::unauthorized(
                "official ChatGPT backend did not accept the client credentials",
            ));
        }
        let catalog: Value = response.json().await.map_err(|_| {
            RouterError::unauthorized("official ChatGPT backend returned an invalid model catalog")
        })?;
        let official_items = official_catalog_models(&catalog)
            .map_err(|_| {
                RouterError::unauthorized(
                    "official ChatGPT backend returned an invalid model catalog",
                )
            })?
            .to_vec();
        let merged = catalog::merge_catalog(&self.config, catalog).map_err(|_| {
            RouterError::unauthorized(
                "external model catalog conflicts with the official model catalog",
            )
        })?;
        self.cache_authorized_catalog(official_items, &merged).await;
        self.degraded_clients.write().await.remove(&fingerprint);
        self.authenticated_clients.write().await.insert(fingerprint);
        Ok(())
    }
}

fn official_catalog_models(value: &Value) -> Result<&[Value]> {
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RouterError::upstream(
                StatusCode::BAD_GATEWAY,
                "official catalog has no models array",
            )
        })?;
    if models.is_empty()
        || models
            .iter()
            .any(|model| catalog::model_id(model).is_none())
    {
        return Err(RouterError::upstream(
            StatusCode::BAD_GATEWAY,
            "official catalog contains no usable model list",
        ));
    }
    Ok(models)
}

fn required_chatgpt_account_digest(headers: &HeaderMap) -> Result<[u8; 32]> {
    if !headers.contains_key("chatgpt-account-id") {
        return Err(RouterError::unauthorized(
            "ChatGPT-Account-ID header is required",
        ));
    }
    catalog_chatgpt_account_digest(headers).ok_or_else(|| {
        RouterError::unauthorized("exactly one non-empty ChatGPT-Account-ID header is required")
    })
}

pub(crate) fn chatgpt_account_generation(headers: &HeaderMap) -> Option<String> {
    let digest = catalog_chatgpt_account_digest(headers)?;
    let mut generation = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut generation, "{byte:02x}").expect("writing a digest to a String cannot fail");
    }
    Some(generation)
}

fn catalog_chatgpt_account_digest(headers: &HeaderMap) -> Option<[u8; 32]> {
    let mut values = headers.get_all("chatgpt-account-id").iter();
    let account_id = values.next()?;
    if account_id.as_bytes().is_empty() || values.next().is_some() {
        return None;
    }
    Some(Sha256::digest(account_id.as_bytes()).into())
}

fn catalog_account_is_eligible(
    account_digest: Option<[u8; 32]>,
    binding_match: Option<bool>,
) -> Option<[u8; 32]> {
    match binding_match {
        Some(false) => None,
        None | Some(true) => account_digest,
    }
}

fn account_binding_mismatch() -> RouterError {
    RouterError::unauthorized("request does not match the ChatGPT account bound to this router")
}

fn client_credential_fingerprint(headers: &HeaderMap) -> Option<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut has_primary_credential = false;
    for name in [
        header::AUTHORIZATION.as_str(),
        header::COOKIE.as_str(),
        "chatgpt-account-id",
    ] {
        let value_count = headers.get_all(name).iter().count();
        if value_count == 0 {
            continue;
        }
        if name == header::AUTHORIZATION.as_str() || name == header::COOKIE.as_str() {
            has_primary_credential = true;
        }
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((value_count as u64).to_be_bytes());
        for value in headers.get_all(name) {
            digest.update((value.as_bytes().len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    has_primary_credential.then(|| digest.finalize().into())
}

async fn responses_ws(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response> {
    reject_cross_origin(&headers)?;
    let upgrade_query = uri.query().map(str::to_owned);
    Ok(upgrade
        .on_upgrade(move |socket| transport::websocket_loop(state, headers, upgrade_query, socket)))
}

pub(crate) fn reject_cross_origin(headers: &HeaderMap) -> Result<()> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let origin = origin
        .to_str()
        .map_err(|_| RouterError::bad_request("invalid Origin header"))?;
    let allowed = [
        "http://127.0.0.1",
        "http://localhost",
        "tauri://localhost",
        "https://tauri.localhost",
    ];
    if allowed
        .iter()
        .any(|prefix| origin == *prefix || origin.starts_with(&format!("{prefix}:")))
    {
        Ok(())
    } else {
        Err(RouterError::bad_request(
            "cross-origin browser requests are not accepted",
        ))
    }
}

pub(crate) fn raw_response(status: StatusCode, content_type: &str, body: bytes::Bytes) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .expect("static response headers are valid")
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use chrono::Utc;
    use cmr_storage::{ModelConfig, ProviderConfig, ResponseRecord, ResponseStatus, StateStore};
    use serde_json::json;

    use super::*;

    #[test]
    fn official_catalog_must_be_nonempty_and_structurally_valid() {
        assert!(official_catalog_models(&json!({"models":[{"slug":"gpt-test"}]})).is_ok());
        assert!(official_catalog_models(&json!({"models":[]})).is_err());
        assert!(
            official_catalog_models(&json!({"models":[{"description":"missing id"}]})).is_err()
        );
        assert!(official_catalog_models(&json!({"not_models":[]})).is_err());
    }

    #[test]
    fn credential_fingerprint_covers_every_repeated_header_value() {
        let mut first = HeaderMap::new();
        first.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer one"),
        );
        first.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer two"),
        );
        first.insert("chatgpt-account-id", HeaderValue::from_static("account"));

        let mut subset = HeaderMap::new();
        subset.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer one"),
        );
        subset.insert("chatgpt-account-id", HeaderValue::from_static("account"));

        assert_ne!(
            client_credential_fingerprint(&first),
            client_credential_fingerprint(&subset)
        );
        assert!(client_credential_fingerprint(&HeaderMap::new()).is_none());
    }

    #[test]
    fn account_digest_requires_exactly_one_nonempty_header() {
        let missing = required_chatgpt_account_digest(&HeaderMap::new()).expect_err("missing");
        assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
        assert!(catalog_chatgpt_account_digest(&HeaderMap::new()).is_none());

        let mut empty = HeaderMap::new();
        empty.insert("chatgpt-account-id", HeaderValue::from_static(""));
        assert!(required_chatgpt_account_digest(&empty).is_err());
        assert!(catalog_chatgpt_account_digest(&empty).is_none());

        let mut repeated = HeaderMap::new();
        repeated.append("chatgpt-account-id", HeaderValue::from_static("workspace"));
        repeated.append("chatgpt-account-id", HeaderValue::from_static("workspace"));
        assert!(required_chatgpt_account_digest(&repeated).is_err());
        assert!(catalog_chatgpt_account_digest(&repeated).is_none());

        let mut present = HeaderMap::new();
        present.insert("chatgpt-account-id", HeaderValue::from_static("workspace"));
        let expected: [u8; 32] = Sha256::digest(b"workspace").into();
        assert_eq!(
            required_chatgpt_account_digest(&present).expect("digest"),
            expected
        );
        assert_eq!(catalog_chatgpt_account_digest(&present), Some(expected));
    }

    #[test]
    fn catalog_injects_only_for_an_eligible_account() {
        let digest = [7_u8; 32];

        assert_eq!(catalog_account_is_eligible(None, None), None);
        assert_eq!(catalog_account_is_eligible(None, Some(true)), None);
        assert_eq!(catalog_account_is_eligible(Some(digest), Some(false)), None);
        assert_eq!(
            catalog_account_is_eligible(Some(digest), None),
            Some(digest)
        );
        assert_eq!(
            catalog_account_is_eligible(Some(digest), Some(true)),
            Some(digest)
        );
    }

    #[tokio::test]
    async fn health_reports_only_authorized_picker_models_after_capacity() {
        let mut config = RouterConfig::default();
        config.providers.push(ProviderConfig {
            id: "external".into(),
            preset: "custom-compatible".into(),
            base_url: Some("https://example.invalid/v1".into()),
            secret_ref: None,
            enabled: true,
            allow_insecure_http: false,
        });
        config.models.extend([
            ModelConfig {
                id: "external-a".into(),
                display_name: "External A".into(),
                provider: "external".into(),
                upstream_model: "external-a".into(),
                order: 1,
                enabled: true,
                context_window: None,
                max_output_tokens: None,
            },
            ModelConfig {
                id: "external-b".into(),
                display_name: "External B".into(),
                provider: "external".into(),
                upstream_model: "external-b".into(),
                order: 0,
                enabled: true,
                context_window: None,
                max_output_tokens: None,
            },
        ]);
        config.catalog_order = vec!["external-b".into(), "external-a".into()];
        config.picker_capacity = 2;

        let app = AppState::new(config.clone(), StateStore::in_memory().expect("state"))
            .expect("app state");
        let Json(cold) = health(State(app.clone())).await;
        assert_eq!(cold["external_models"], json!([]));
        assert_eq!(
            cold["routable_external_models"],
            json!(["external-a", "external-b"])
        );
        assert_eq!(cold["official_catalog_cached"], false);

        let official_items = vec![json!({"slug":"gpt-a"})];
        let merged = catalog::merge_catalog(&config, json!({"models": official_items.clone()}))
            .expect("merge");
        app.cache_authorized_catalog(official_items, &merged).await;

        let Json(warm) = health(State(app)).await;
        assert_eq!(warm["external_models"], json!(["external-b"]));
        assert_eq!(
            warm["routable_external_models"],
            json!(["external-a", "external-b"])
        );
        assert_eq!(warm["official_catalog_cached"], true);
    }

    #[test]
    fn app_state_recovers_interrupted_streams_before_serving() {
        let sessions = StateStore::in_memory().expect("state");
        let created_at = Utc::now();
        sessions
            .begin_response(&ResponseRecord {
                id: "resp_interrupted".into(),
                session_id: "session_interrupted".into(),
                previous_response_id: None,
                provider_id: "official".into(),
                provider_owner_id: None,
                model_id: "gpt-test".into(),
                input: vec![json!({"type":"message","role":"user","content":"run tool"})],
                output: Vec::new(),
                status: ResponseStatus::InProgress,
                incomplete_details: None,
                created_at,
            })
            .expect("begin response");
        let call = json!({
            "type":"function_call",
            "id":"fc_interrupted",
            "call_id":"call_interrupted",
            "name":"tool",
            "arguments":"{}"
        });
        sessions
            .journal_function_call("resp_interrupted", 0, &call)
            .expect("journal function call");

        let app = AppState::new(RouterConfig::default(), sessions).expect("app state");
        let recovered = app
            .sessions
            .response("resp_interrupted")
            .expect("load recovered response")
            .expect("recovered response");
        assert_eq!(recovered.status, ResponseStatus::Incomplete);
        assert_eq!(
            recovered.incomplete_details,
            Some(json!({"reason":"router_restart"}))
        );
        assert_eq!(recovered.output, vec![call]);
        assert_eq!(recovered.created_at, created_at);
    }
}
