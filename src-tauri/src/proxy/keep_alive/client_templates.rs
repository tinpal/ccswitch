//! Obtain the installed clients' actual HTTP requests once, then reuse their
//! transport settings for background inference with either the full native
//! prompt or a short prompt and native client identity.
//! No synthetic client profile or generic API fallback is used. The capture
//! listener never forwards traffic.

use axum::{
    extract::{DefaultBodyLimit, OriginalUri, State},
    Json, Router,
};
use http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use process_wrap::tokio::{KillOnDrop, TokioChildWrapper, TokioCommandWrap};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncWriteExt},
    net::TcpListener,
    process::Command,
    sync::{oneshot, Mutex, OnceCell},
    task::AbortHandle,
};
use uuid::Uuid;

use super::prompt::{minimize_prompt, PROMPT};

const MAX_TEMPLATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHED_TEMPLATES: usize = 128;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) enum ClientKind {
    Codex,
    Claude,
}

impl ClientKind {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct TemplateKey {
    client: ClientKind,
    model: String,
    bearer: bool,
    use_minimal_prompt: bool,
    executable: PathBuf,
    executable_bytes: u64,
    executable_modified: Option<SystemTime>,
}

#[derive(Clone)]
struct RequestTemplate {
    endpoint: String,
    headers: HeaderMap,
    body: Value,
}

pub(super) struct NativeRequest {
    pub endpoint: String,
    pub headers: HeaderMap,
    pub body: Value,
    pub session_id: String,
}

pub(super) struct ClientTemplates {
    entries: Mutex<HashMap<TemplateKey, Arc<OnceCell<RequestTemplate>>>>,
}

impl Default for ClientTemplates {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl ClientTemplates {
    pub async fn prepare(
        &self,
        client: ClientKind,
        model: &str,
        bearer: bool,
        use_minimal_prompt: bool,
        timeout: u32,
    ) -> Result<NativeRequest, &'static str> {
        let executable = find_executable(client).ok_or("client_not_found")?;
        let metadata = std::fs::metadata(&executable).map_err(|_| "client_not_found")?;
        let key = TemplateKey {
            client,
            model: model.into(),
            bearer,
            use_minimal_prompt,
            executable: executable.clone(),
            executable_bytes: metadata.len(),
            executable_modified: metadata.modified().ok(),
        };
        let cell = {
            let mut entries = self.entries.lock().await;
            // Cache full and minimal prompts separately. A client update
            // invalidates only the matching client's model and prompt mode.
            entries.retain(|old, _| {
                old.client != client
                    || old.model != model
                    || old.bearer != bearer
                    || old.use_minimal_prompt != use_minimal_prompt
                    || *old == key
            });
            if !entries.contains_key(&key) && entries.len() >= MAX_CACHED_TEMPLATES {
                if let Some(old) = entries.keys().next().cloned() {
                    entries.remove(&old);
                }
            }
            entries
                .entry(key)
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let template = cell
            .get_or_try_init(|| {
                capture(client, &executable, model, bearer, use_minimal_prompt, timeout)
            })
            .await?;
        Ok(template.fresh(client))
    }
}

impl RequestTemplate {
    fn fresh(&self, client: ClientKind) -> NativeRequest {
        let mut headers = self.headers.clone();
        let mut body = self.body.clone();
        let session_id = match client {
            ClientKind::Codex => Uuid::now_v7().to_string(),
            ClientKind::Claude => Uuid::new_v4().to_string(),
        };

        match client {
            ClientKind::Codex => {
                let mut replacements: Vec<(String, String)> = Vec::new();
                for name in ["session-id", "session_id"] {
                    if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
                        replacements.push((value.to_string(), session_id.clone()));
                    }
                }
                if let Some(meta) = body.get("client_metadata").and_then(Value::as_object) {
                    for (name, fresh) in [
                        ("session_id", session_id.clone()),
                        ("thread_id", session_id.clone()),
                        ("turn_id", Uuid::now_v7().to_string()),
                    ] {
                        if let Some(old) = meta.get(name).and_then(Value::as_str) {
                            replacements.push((old.to_string(), fresh));
                        }
                    }
                    if let Some(turn) = meta
                        .get("x-codex-turn-metadata")
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    {
                        if let Some(old) = turn.get("context_window_id").and_then(Value::as_str) {
                            replacements.push((old.into(), Uuid::now_v7().to_string()));
                        }
                    }
                }
                replace_identifiers(&mut body, &replacements);
                for value in headers.values_mut() {
                    if let Ok(old) = value.to_str() {
                        let fresh = replace_text(old, &replacements);
                        if let Ok(fresh) = HeaderValue::from_str(&fresh) {
                            *value = fresh;
                        }
                    }
                }
                if let Some(raw) = body
                    .pointer_mut("/client_metadata/x-codex-turn-metadata")
                    .filter(|value| value.is_string())
                {
                    if let Ok(mut turn) = serde_json::from_str::<Value>(raw.as_str().unwrap_or(""))
                    {
                        if turn.get("turn_started_at_unix_ms").is_some() {
                            turn["turn_started_at_unix_ms"] =
                                json!(chrono::Utc::now().timestamp_millis());
                        }
                        *raw = Value::String(turn.to_string());
                    }
                }
                if body.get("prompt_cache_key").is_some() {
                    body["prompt_cache_key"] = json!(session_id);
                }
            }
            ClientKind::Claude => {
                // Keep the installed client's device metadata; create a fresh
                // session without replaying an interactive conversation.
                if let Some(raw) = body.pointer_mut("/metadata/user_id") {
                    if let Some(mut identity) = raw
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    {
                        if identity.get("session_id").is_some() {
                            identity["session_id"] = json!(session_id);
                        }
                        *raw = Value::String(identity.to_string());
                    }
                }
            }
        }
        NativeRequest {
            endpoint: self.endpoint.clone(),
            headers,
            body,
            session_id,
        }
    }
}

fn replace_text(text: &str, replacements: &[(String, String)]) -> String {
    replacements
        .iter()
        .fold(text.to_string(), |text, (old, fresh)| {
            if old.is_empty() {
                text
            } else {
                text.replace(old, fresh)
            }
        })
}

fn replace_identifiers(value: &mut Value, replacements: &[(String, String)]) {
    match value {
        Value::String(text) => *text = replace_text(text, replacements),
        Value::Array(values) => {
            for value in values {
                replace_identifiers(value, replacements);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                replace_identifiers(value, replacements);
            }
        }
        _ => {}
    }
}

#[derive(Clone)]
struct CaptureState {
    client: ClientKind,
    use_minimal_prompt: bool,
    nonce: String,
    sender: Arc<StdMutex<Option<oneshot::Sender<RequestTemplate>>>>,
}

async fn capture_request(
    State(state): State<CaptureState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let authenticated = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        == Some(state.nonce.as_str())
        || headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            == Some(state.nonce.as_str());
    if !authenticated {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "Local capture only"}})),
        );
    }
    if uri.path().ends_with("/messages/count_tokens") {
        return (StatusCode::OK, Json(json!({"input_tokens": 1})));
    }
    let expected = match state.client {
        ClientKind::Codex => "/v1/responses",
        ClientKind::Claude => "/v1/messages",
    };
    if method != Method::POST || uri.path() != expected {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"message": "Local capture only"}})),
        );
    }
    if let Ok(mut body) = serde_json::from_slice::<Value>(&bytes) {
        if body
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|model| !model.is_empty())
            && body.get("stream").and_then(Value::as_bool) == Some(true)
            && !body.to_string().contains(&state.nonce)
        {
            if state.use_minimal_prompt {
                // Cache only the short prompt and recognized native identity
                // when minimization is enabled for this model.
                minimize_prompt(&mut body);
            }
            let mut public_headers = HeaderMap::new();
            for (name, value) in &headers {
                if is_public_template_header(name.as_str()) {
                    public_headers.append(name.clone(), value.clone());
                }
            }
            let endpoint = uri.path_and_query().map_or(expected, |path| path.as_str());
            let endpoint = if state.client == ClientKind::Codex {
                endpoint.strip_prefix("/v1").unwrap_or(endpoint)
            } else {
                endpoint
            };
            if let Some(sender) = state
                .sender
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let _ = sender.send(RequestTemplate {
                    endpoint: endpoint.into(),
                    headers: public_headers,
                    body,
                });
            }
        }
    }
    // End the inert request before inference. The installed client is stopped as
    // soon as the request is captured; this never reaches a provider.
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(
            json!({"error": {"type": "server_error", "message": "Local request template captured"}}),
        ),
    )
}

fn is_public_template_header(name: &str) -> bool {
    !matches!(
        name,
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "cookie"
            | "set-cookie"
            | "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
            | "chatgpt-account-id"
            | "openai-organization"
            | "openai-project"
    ) && !name.starts_with("x-oai-")
        && !name.starts_with("x-openai-")
}

struct CaptureServer(AbortHandle);
impl Drop for CaptureServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct CaptureProcess {
    child: Option<Box<dyn TokioChildWrapper>>,
    workspace: Option<TempDir>,
    drains: Vec<tokio::task::JoinHandle<()>>,
}

impl CaptureProcess {
    async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), Box::into_pin(child.wait())).await;
        }
        self.workspace.take();
        for drain in self.drains.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
        }
    }
}

impl Drop for CaptureProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let workspace = self.workspace.take();
            let drains = std::mem::take(&mut self.drains);
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ =
                    tokio::time::timeout(Duration::from_secs(5), Box::into_pin(child.wait())).await;
                drop(child);
                drop(workspace);
                for drain in drains {
                    let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
                }
            });
        }
    }
}

async fn drain_client_output<R: AsyncRead + Unpin>(mut stream: R) {
    let _ = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
}

async fn capture(
    client: ClientKind,
    executable: &Path,
    model: &str,
    bearer: bool,
    use_minimal_prompt: bool,
    timeout: u32,
) -> Result<RequestTemplate, &'static str> {
    let workspace = tempfile::Builder::new()
        .prefix("cc-switch-native-request-")
        .tempdir()
        .map_err(|_| "client_template_failed")?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "client_template_failed")?;
    let address = listener
        .local_addr()
        .map_err(|_| "client_template_failed")?;
    let nonce = format!("cc-switch-local-{}", Uuid::new_v4());
    let (sender, receiver) = oneshot::channel();
    let router = Router::new()
        .fallback(capture_request)
        .layer(DefaultBodyLimit::max(MAX_TEMPLATE_BYTES))
        .with_state(CaptureState {
            client,
            use_minimal_prompt,
            nonce: nonce.clone(),
            sender: Arc::new(StdMutex::new(Some(sender))),
        });
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let _server_guard = CaptureServer(server.abort_handle());
    let command = native_command(
        client,
        executable,
        model,
        bearer,
        &nonce,
        address.port(),
        workspace.path(),
    );
    let mut command = TokioCommandWrap::from(command);
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    {
        let mut flags = process_wrap::tokio::CreationFlags(Default::default());
        flags.0 .0 = 0x08000000; // CREATE_NO_WINDOW
        command.wrap(flags).wrap(process_wrap::tokio::JobObject);
    }
    let mut child = command.spawn().map_err(|_| "client_template_failed")?;
    let stdout = child
        .inner_mut()
        .stdout
        .take()
        .ok_or("client_template_failed")?;
    let stderr = child
        .inner_mut()
        .stderr
        .take()
        .ok_or("client_template_failed")?;
    let mut process = CaptureProcess {
        child: Some(child),
        workspace: Some(workspace),
        drains: vec![
            tokio::spawn(drain_client_output(stdout)),
            tokio::spawn(drain_client_output(stderr)),
        ],
    };
    let mut stdin = process
        .child
        .as_mut()
        .unwrap()
        .inner_mut()
        .stdin
        .take()
        .ok_or("client_template_failed")?;
    stdin
        .write_all(PROMPT.as_bytes())
        .await
        .map_err(|_| "client_template_failed")?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|_| "client_template_failed")?;
    drop(stdin);
    let result = {
        let child = process.child.as_mut().unwrap();
        tokio::select! {
            biased;
            template = receiver => template.map_err(|_| "client_template_failed"),
            _ = child.inner_mut().wait() => Err("client_template_failed"),
            _ = tokio::time::sleep(Duration::from_secs(timeout.clamp(1, 20) as u64)) => Err("client_template_timeout"),
        }
    };
    process.stop().await;
    server.abort();
    let _ = server.await;
    if let Ok(template) = &result {
        let agent = template
            .headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown");
        log::info!(
            "[Model KeepAlive] App={} Model={} Event=template_ready Client={}",
            client.name(),
            model,
            agent
        );
    }
    result
}

fn native_command(
    client: ClientKind,
    executable: &Path,
    model: &str,
    bearer: bool,
    nonce: &str,
    port: u16,
    workspace: &Path,
) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, _) in std::env::vars_os() {
        let key = name.to_string_lossy().to_ascii_uppercase();
        if key.starts_with("OPENAI_")
            || key.starts_with("ANTHROPIC_")
            || key.starts_with("CLAUDE_")
            || key.starts_with("CLAUDECODE")
            || (key.starts_with("CODEX_") && key != "CODEX_HOME")
        {
            command.env_remove(name);
        }
    }
    command.env("NO_COLOR", "1");
    match client {
        ClientKind::Codex => {
            command.env("CC_SWITCH_TEMPLATE_KEY", nonce).args([
                "exec",
                "--json",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "--color",
                "never",
                "--model",
                model,
            ]);
            let options = [
                ("model_provider", json!("cc_switch_template").to_string()),
                (
                    "model_providers.cc_switch_template.name",
                    json!("CC Switch local template capture").to_string(),
                ),
                (
                    "model_providers.cc_switch_template.base_url",
                    json!(format!("http://127.0.0.1:{port}/v1")).to_string(),
                ),
                (
                    "model_providers.cc_switch_template.env_key",
                    json!("CC_SWITCH_TEMPLATE_KEY").to_string(),
                ),
                (
                    "model_providers.cc_switch_template.wire_api",
                    json!("responses").to_string(),
                ),
                (
                    "model_providers.cc_switch_template.requires_openai_auth",
                    "false".into(),
                ),
                (
                    "model_providers.cc_switch_template.supports_websockets",
                    "false".into(),
                ),
                (
                    "model_providers.cc_switch_template.request_max_retries",
                    "0".into(),
                ),
                (
                    "model_providers.cc_switch_template.stream_max_retries",
                    "0".into(),
                ),
                ("approval_policy", json!("never").to_string()),
                ("web_search", json!("disabled").to_string()),
                ("features.shell_tool", "false".into()),
                ("features.apps", "false".into()),
                ("features.hooks", "false".into()),
                ("features.plugins", "false".into()),
                ("features.remote_plugin", "false".into()),
                ("features.skip_host_skill_discovery", "true".into()),
                ("project_doc_max_bytes", "0".into()),
                ("suppress_unstable_features_warning", "true".into()),
            ];
            for (name, value) in options {
                command.args(["-c", &format!("{name}={value}")]);
            }
            command.arg("-");
        }
        ClientKind::Claude => {
            let auth = if bearer {
                "ANTHROPIC_AUTH_TOKEN"
            } else {
                "ANTHROPIC_API_KEY"
            };
            command
                .env(auth, nonce)
                .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
                .env("CLAUDE_CODE_MAX_RETRIES", "0")
                .args([
                    "--print",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--model",
                    model,
                    "--effort",
                    "low",
                    "--max-turns",
                    "1",
                    "--no-session-persistence",
                    "--safe-mode",
                    "--setting-sources",
                    "",
                    "--tools",
                    "",
                    "--permission-mode",
                    "dontAsk",
                    "--strict-mcp-config",
                    "--mcp-config",
                    "{\"mcpServers\":{}}",
                ]);
        }
    }
    command
}

fn find_executable(client: ClientKind) -> Option<PathBuf> {
    let override_name = match client {
        ClientKind::Codex => "CC_SWITCH_CODEX_EXECUTABLE",
        ClientKind::Claude => "CC_SWITCH_CLAUDE_EXECUTABLE",
    };
    if let Some(path) = std::env::var_os(override_name).map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let program = format!("{}{}", client.name(), std::env::consts::EXE_SUFFIX);
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    for directory in &path_dirs {
        // Resolve the first npm CLI shim before an unrelated bundled binary
        // later on PATH, matching the user's normal Codex command.
        if cfg!(windows)
            && client == ClientKind::Codex
            && (directory.join("codex.ps1").is_file() || directory.join("codex.cmd").is_file())
        {
            if let Some(path) = npm_codex_executable(directory) {
                return Some(path);
            }
        }
        let path = directory.join(&program);
        if path.is_file() {
            return Some(path);
        }
    }
    if client == ClientKind::Claude {
        if let Some(home) = dirs::home_dir() {
            let path = home.join(".local/bin").join(&program);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    if client == ClientKind::Codex {
        let mut roots = path_dirs;
        if let Some(app_data) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(app_data).join("npm"));
        }
        for root in roots {
            if let Some(path) = npm_codex_executable(&root) {
                return Some(path);
            }
        }
    }
    None
}

fn npm_codex_executable(root: &Path) -> Option<PathBuf> {
    let program = format!("codex{}", std::env::consts::EXE_SUFFIX);
    let (target, package) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", "codex-win32-x64"),
        ("windows", "aarch64") => ("aarch64-pc-windows-msvc", "codex-win32-arm64"),
        ("macos", "aarch64") => ("aarch64-apple-darwin", "codex-darwin-arm64"),
        ("macos", _) => ("x86_64-apple-darwin", "codex-darwin-x64"),
        ("linux", "aarch64") => ("aarch64-unknown-linux-musl", "codex-linux-arm64"),
        _ => ("x86_64-unknown-linux-musl", "codex-linux-x64"),
    };
    let base = root.join("node_modules/@openai/codex");
    for relative in [
        format!("node_modules/@openai/{package}/vendor/{target}/bin/{program}"),
        format!("node_modules/@openai/{package}/vendor/{target}/codex/{program}"),
        format!("vendor/{target}/codex/{program}"),
        format!("vendor/{target}/bin/{program}"),
    ] {
        let path = base.join(relative);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}
