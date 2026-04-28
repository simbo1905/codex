use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header::AUTHORIZATION;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::build_reqwest_client;
use rand::RngCore as _;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;

const SCANNER_PATH_PREFIX: &str = "/agent-security-scanner/";
const FACADE_PATH_PREFIX: &str = "/api/codex/agent-security-scanner";
const STATE_FILE_NAME: &str = "agent-security-scanner.host-bridge.json";

#[derive(Clone)]
struct AgentSecurityScannerBridgeState {
    auth_manager: Arc<AuthManager>,
    chatgpt_base_url: String,
    client: reqwest::Client,
    token: String,
}

#[derive(Deserialize, Serialize)]
struct AgentSecurityScannerBridgeStateFile {
    base_url: String,
    token: String,
}

pub(crate) async fn start_agent_security_scanner_bridge(
    codex_home: PathBuf,
    chatgpt_base_url: String,
    auth_manager: Arc<AuthManager>,
    shutdown_token: CancellationToken,
) -> io::Result<JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let local_addr = listener.local_addr()?;
    let state = AgentSecurityScannerBridgeStateFile {
        base_url: format!("http://{local_addr}"),
        token: random_bridge_token(),
    };
    let state_file_path = codex_home.join(STATE_FILE_NAME);

    write_state_file(&codex_home, &state_file_path, &state).await?;

    let router = Router::new()
        .fallback(any(handle_agent_security_scanner_bridge_request))
        .with_state(AgentSecurityScannerBridgeState {
            auth_manager,
            chatgpt_base_url,
            client: build_reqwest_client(),
            token: state.token.clone(),
        });
    let server = axum::serve(listener, router).with_graceful_shutdown({
        let shutdown_token = shutdown_token.clone();
        async move {
            shutdown_token.cancelled().await;
        }
    });

    info!(
        base_url = state.base_url,
        state_file_path = %state_file_path.display(),
        "agent security scanner bridge listening"
    );
    Ok(tokio::spawn(async move {
        if let Err(err) = server.await {
            error!("agent security scanner bridge failed: {err}");
        }
        if let Err(err) = remove_state_file_if_owned(&state_file_path, &state).await {
            error!(
                state_file_path = %state_file_path.display(),
                "failed to remove agent security scanner bridge state file: {err}"
            );
        }
        info!("agent security scanner bridge shutting down");
    }))
}

async fn write_state_file(
    codex_home: &Path,
    state_file_path: &Path,
    state: &AgentSecurityScannerBridgeStateFile,
) -> io::Result<()> {
    fs::create_dir_all(codex_home).await?;
    let state_file = serde_json::to_vec(state).map_err(io::Error::other)?;
    fs::write(state_file_path, state_file).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(state_file_path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}

async fn remove_state_file_if_owned(
    state_file_path: &Path,
    expected_state: &AgentSecurityScannerBridgeStateFile,
) -> io::Result<()> {
    let payload = match fs::read(state_file_path).await {
        Ok(payload) => payload,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let current_state = serde_json::from_slice::<AgentSecurityScannerBridgeStateFile>(&payload)
        .map_err(io::Error::other)?;
    if current_state.base_url == expected_state.base_url
        && current_state.token == expected_state.token
    {
        fs::remove_file(state_file_path).await?;
    }
    Ok(())
}

async fn handle_agent_security_scanner_bridge_request(
    State(state): State<AgentSecurityScannerBridgeState>,
    request: Request,
) -> Response {
    if !has_valid_bridge_token(request.headers(), &state.token) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "Unauthorized host bridge request.",
        );
    }
    if request.method() != Method::GET && request.method() != Method::POST {
        return json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Unsupported scanner bridge method.",
        );
    }
    let Some(facade_path) = build_agent_security_scanner_facade_path(request.uri()) else {
        return json_error(StatusCode::NOT_FOUND, "Unknown scanner bridge path.");
    };
    let method = request.method().clone();
    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to read scanner bridge request body: {err}"),
            );
        }
    };

    match forward_to_facade(&state, method, facade_path, body).await {
        Ok(response) => response,
        Err(err) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Host bridge request failed: {err}"),
        ),
    }
}

fn has_valid_bridge_token(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {token}"))
}

fn build_agent_security_scanner_facade_path(uri: &Uri) -> Option<String> {
    let path = uri.path();
    if !path.starts_with(SCANNER_PATH_PREFIX) {
        return None;
    }
    let mut facade_path = format!(
        "{FACADE_PATH_PREFIX}{}",
        &path[SCANNER_PATH_PREFIX.len().saturating_sub(1)..]
    );
    if let Some(query) = uri.query() {
        facade_path.push('?');
        facade_path.push_str(query);
    }
    Some(facade_path)
}

fn scanner_facade_url(chatgpt_base_url: &str, facade_path: &str) -> String {
    let base_url = chatgpt_base_url.trim_end_matches('/');
    let facade_base_url = base_url.strip_suffix("/backend-api").unwrap_or(base_url);
    format!(
        "{}/{}",
        facade_base_url,
        facade_path.trim_start_matches('/')
    )
}

async fn forward_to_facade(
    state: &AgentSecurityScannerBridgeState,
    method: Method,
    facade_path: String,
    body: axum::body::Bytes,
) -> io::Result<Response> {
    let mut auth_recovery = state.auth_manager.unauthorized_recovery();
    loop {
        let auth = scanner_auth(&state.auth_manager).await?;
        let response = send_facade_request(state, &auth, &method, &facade_path, body.clone())
            .await
            .map_err(io::Error::other)?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED || !auth_recovery.has_next() {
            return response_from_reqwest(response).await;
        }
        auth_recovery.next().await.map_err(io::Error::other)?;
    }
}

async fn scanner_auth(auth_manager: &Arc<AuthManager>) -> io::Result<CodexAuth> {
    let Some(auth) = auth_manager.auth().await else {
        return Err(io::Error::other(
            "Sign in to ChatGPT in Codex to call Agent Security Scanner.",
        ));
    };
    if !auth.uses_codex_backend() {
        return Err(io::Error::other(
            "ChatGPT authentication is required to call Agent Security Scanner.",
        ));
    }
    Ok(auth)
}

async fn send_facade_request(
    state: &AgentSecurityScannerBridgeState,
    auth: &CodexAuth,
    method: &Method,
    facade_path: &str,
    body: axum::body::Bytes,
) -> reqwest::Result<reqwest::Response> {
    let mut headers = codex_model_provider::auth_provider_from_auth(auth).to_auth_headers();
    if !body.is_empty() {
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    state
        .client
        .request(
            method.clone(),
            scanner_facade_url(&state.chatgpt_base_url, facade_path),
        )
        .headers(headers)
        .body(body)
        .send()
        .await
}

async fn response_from_reqwest(response: reqwest::Response) -> io::Result<Response> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let body = response.bytes().await.map_err(io::Error::other)?;
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
    }
    builder.body(Body::from(body)).map_err(io::Error::other)
}

fn json_error(status: StatusCode, detail: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "detail": detail,
        })),
    )
        .into_response()
}

fn random_bridge_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::routing::any;
    use codex_config::types::AuthCredentialsStoreMode;
    use codex_core::test_support::auth_manager_from_auth;
    use codex_login::ExternalAuth;
    use codex_login::ExternalAuthRefreshContext;
    use codex_login::ExternalAuthTokens;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[test]
    fn maps_only_scanner_paths_to_facade_paths() {
        let uri: Uri = "/agent-security-scanner/backend-conversations/abc?include=true"
            .parse()
            .expect("valid uri");
        assert_eq!(
            build_agent_security_scanner_facade_path(&uri),
            Some(
                "/api/codex/agent-security-scanner/backend-conversations/abc?include=true"
                    .to_string()
            )
        );
        let unrelated_uri: Uri = "/not-scanner".parse().expect("valid uri");
        assert_eq!(
            build_agent_security_scanner_facade_path(&unrelated_uri),
            None
        );
    }

    #[test]
    fn strips_backend_api_before_building_facade_url() {
        assert_eq!(
            scanner_facade_url(
                "https://chatgpt.com/backend-api/",
                "/api/codex/agent-security-scanner/auth-test",
            ),
            "https://chatgpt.com/api/codex/agent-security-scanner/auth-test"
        );
        assert_eq!(
            scanner_facade_url(
                "http://127.0.0.1:8061",
                "/api/codex/agent-security-scanner/auth-test",
            ),
            "http://127.0.0.1:8061/api/codex/agent-security-scanner/auth-test"
        );
    }

    #[tokio::test]
    async fn rejects_requests_without_the_bridge_token() {
        let codex_home = TempDir::new().expect("temp dir should exist");
        let state = AgentSecurityScannerBridgeState {
            auth_manager: AuthManager::shared(
                codex_home.path().to_path_buf(),
                /*enable_codex_api_key_env*/ false,
                codex_config::types::AuthCredentialsStoreMode::Ephemeral,
                /*chatgpt_base_url*/ None,
            )
            .await,
            chatgpt_base_url: "https://chatgpt.com/backend-api/".to_string(),
            client: build_reqwest_client(),
            token: "bridge-token".to_string(),
        };
        let request = Request::builder()
            .uri("/agent-security-scanner/auth-test")
            .body(Body::empty())
            .expect("request should build");

        let response = handle_agent_security_scanner_bridge_request(State(state), request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("valid json"),
            serde_json::json!({"detail": "Unauthorized host bridge request."})
        );
    }

    #[tokio::test]
    async fn removes_only_owned_state_files() {
        let codex_home = TempDir::new().expect("temp dir should exist");
        let state_file_path = codex_home.path().join(STATE_FILE_NAME);
        let owned_state = AgentSecurityScannerBridgeStateFile {
            base_url: "http://127.0.0.1:1111".to_string(),
            token: "owned".to_string(),
        };
        let replacement_state = AgentSecurityScannerBridgeStateFile {
            base_url: "http://127.0.0.1:2222".to_string(),
            token: "replacement".to_string(),
        };

        write_state_file(codex_home.path(), &state_file_path, &replacement_state)
            .await
            .expect("state file should write");
        remove_state_file_if_owned(&state_file_path, &owned_state)
            .await
            .expect("foreign state should be preserved");
        assert!(state_file_path.exists());

        write_state_file(codex_home.path(), &state_file_path, &owned_state)
            .await
            .expect("state file should rewrite");
        remove_state_file_if_owned(&state_file_path, &owned_state)
            .await
            .expect("owned state should be removed");
        assert!(!state_file_path.exists());
    }

    #[tokio::test]
    async fn forwards_host_auth_headers_to_the_facade() {
        let captured_headers = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let (base_url, server_handle) = spawn_test_facade({
            let captured_headers = Arc::clone(&captured_headers);
            move |headers| {
                captured_headers
                    .lock()
                    .expect("capture lock should not be poisoned")
                    .push(headers);
                StatusCode::OK
            }
        })
        .await;
        let state = AgentSecurityScannerBridgeState {
            auth_manager: auth_manager_from_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            chatgpt_base_url: base_url,
            client: build_reqwest_client(),
            token: "bridge-token".to_string(),
        };

        let response = forward_to_facade(
            &state,
            Method::GET,
            "/api/codex/agent-security-scanner/auth-test".to_string(),
            axum::body::Bytes::new(),
        )
        .await
        .expect("forward should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        let headers = captured_headers
            .lock()
            .expect("capture lock should not be poisoned");
        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers[0]
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer Access Token")
        );
        assert_eq!(
            headers[0]
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok()),
            Some("account_id")
        );
        server_handle.abort();
    }

    #[tokio::test]
    async fn retries_once_with_refreshed_external_chatgpt_auth_after_401() {
        let codex_home = TempDir::new().expect("temp dir should exist");
        let stale_token = fake_jwt("stale-token@example.com");
        let fresh_token = fake_jwt("fresh-token@example.com");
        codex_login::auth::login_with_chatgpt_auth_tokens(
            codex_home.path(),
            &stale_token,
            "account_id",
            None,
        )
        .expect("external chatgpt auth should save");
        let auth_manager = AuthManager::shared(
            codex_home.path().to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            AuthCredentialsStoreMode::Ephemeral,
            /*chatgpt_base_url*/ None,
        )
        .await;
        auth_manager.set_external_auth(Arc::new(TestExternalAuth {
            fresh_token: fresh_token.clone(),
        }));

        let captured_headers = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let (base_url, server_handle) = spawn_test_facade({
            let captured_headers = Arc::clone(&captured_headers);
            move |headers| {
                let mut captured = captured_headers
                    .lock()
                    .expect("capture lock should not be poisoned");
                captured.push(headers);
                if captured.len() == 1 {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::OK
                }
            }
        })
        .await;
        let state = AgentSecurityScannerBridgeState {
            auth_manager,
            chatgpt_base_url: base_url,
            client: build_reqwest_client(),
            token: "bridge-token".to_string(),
        };

        let response = forward_to_facade(
            &state,
            Method::GET,
            "/api/codex/agent-security-scanner/auth-test".to_string(),
            axum::body::Bytes::new(),
        )
        .await
        .expect("forward should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        let headers = captured_headers
            .lock()
            .expect("capture lock should not be poisoned");
        assert_eq!(headers.len(), 2);
        let expected_stale_header = format!("Bearer {stale_token}");
        let expected_fresh_header = format!("Bearer {fresh_token}");
        assert_eq!(
            headers[0]
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some(expected_stale_header.as_str())
        );
        assert_eq!(
            headers[1]
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some(expected_fresh_header.as_str())
        );
        server_handle.abort();
    }

    #[derive(Clone)]
    struct TestExternalAuth {
        fresh_token: String,
    }

    #[async_trait]
    impl ExternalAuth for TestExternalAuth {
        fn auth_mode(&self) -> codex_app_server_protocol::AuthMode {
            codex_app_server_protocol::AuthMode::Chatgpt
        }

        async fn refresh(
            &self,
            _context: ExternalAuthRefreshContext,
        ) -> io::Result<ExternalAuthTokens> {
            Ok(ExternalAuthTokens::chatgpt(
                self.fresh_token.clone(),
                "account_id",
                None,
            ))
        }
    }

    fn fake_jwt(email: &str) -> String {
        let header = serde_json::json!({ "alg": "none", "typ": "JWT" });
        let payload = serde_json::json!({ "email": email });
        let header_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header should serialize"));
        let payload_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload should serialize"));
        let signature_b64 = URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header_b64}.{payload_b64}.{signature_b64}")
    }

    async fn spawn_test_facade(
        handler: impl Fn(HeaderMap) -> StatusCode + Clone + Send + Sync + 'static,
    ) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let router = Router::new().route(
            "/api/codex/agent-security-scanner/auth-test",
            any(move |headers: HeaderMap| {
                let handler = handler.clone();
                async move { handler(headers) }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("server should run");
        });
        (format!("http://{address}"), handle)
    }
}
