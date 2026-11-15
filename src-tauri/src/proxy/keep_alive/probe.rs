use crate::proxy::{
    content_encoding::get_content_encoding,
    forwarder::RequestForwarder,
    server::ProxyState,
    types::{CopilotOptimizerConfig, OptimizerConfig, RectifierConfig},
    ProxyError,
};
use crate::{app_config::AppType, provider::Provider};
use http::{header, Extensions};
use serde_json::Value;

use super::{client_templates::ClientKind, update, NativeClientRequest, SharedStatus};

pub(super) struct ProbeError {
    /// Fixed categories only: never surface a raw upstream body, URL or secret.
    pub code: &'static str,
    pub http_status: Option<u16>,
    /// Kept only for the redacted diagnostic store, never for status/tooltips.
    pub detail: Option<String>,
}

impl ProbeError {
    pub fn new(code: &'static str, http_status: Option<u16>) -> Self {
        Self { code, http_status, detail: None }
    }

    fn from_proxy(error: ProxyError) -> Self {
        let detail = match &error {
            ProxyError::UpstreamError { body, .. } => body.clone(),
            _ => Some(error.to_string()),
        };
        let mut result = match error {
            ProxyError::UpstreamError { status, .. } => {
                Self::new("upstream_http_error", Some(status))
            }
            ProxyError::AuthError(_) | ProxyError::CodexOfficialLoginPaused => {
                Self::new("authentication_error", None)
            }
            ProxyError::ConfigError(_)
            | ProxyError::NoAvailableProvider
            | ProxyError::NoProvidersConfigured => Self::new("configuration_error", None),
            ProxyError::Timeout(_) | ProxyError::StreamIdleTimeout(_) => Self::new("timeout", None),
            ProxyError::ResponseBodyTooLarge(_) => Self::new("response_too_large", None),
            _ => Self::new("request_failed", None),
        };
        result.detail = detail;
        result
    }
}

pub(super) async fn request(
    state: &ProxyState,
    app: &AppType,
    provider: &Provider,
    model: &str,
    timeout: u32,
    status: &SharedStatus,
    response_meta: &crate::proxy::upstream_errors::UpstreamResponseMeta,
) -> Result<u16, ProbeError> {
    let request_model = if matches!(app, AppType::Claude) {
        crate::mode::stack::claude_models("", provider)
            .into_iter()
            .find(|entry| {
                crate::proxy::model_mapper::strip_one_m_suffix_for_upstream(&entry.upstream).trim()
                    == crate::proxy::model_mapper::strip_one_m_suffix_for_upstream(model).trim()
            })
            .map(|entry| entry.upstream)
            .unwrap_or_else(|| model.to_string())
    } else {
        model.to_string()
    };
    let client = match app {
        AppType::Claude => ClientKind::Claude,
        AppType::Codex => ClientKind::Codex,
        _ => return Err(ProbeError::new("unsupported_app", None)),
    };
    let bearer = provider
        .settings_config
        .pointer("/env/ANTHROPIC_AUTH_TOKEN")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    update(status, |s| {
        s.reason = Some("preparing_client_template".into())
    });
    let native = state
        .keep_alive
        .client_templates
        .prepare(client, &request_model, bearer, timeout)
        .await;
    update(status, |s| s.reason = None);
    let native = native.map_err(|code| ProbeError::new(code, None))?;
    update(status, |s| {
        s.client_user_agent = native
            .headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
    });
    // The installed client generated the complete request. Only fresh background
    // identifiers and the provider's own authentication are substituted.
    let forwarder = RequestForwarder::new(
        state.provider_router.clone(),
        timeout as u64,
        state.status.clone(),
        state.current_providers.clone(),
        state.gemini_shadow.clone(),
        state.codex_chat_history.clone(),
        state.failover_manager.clone(),
        state.app_handle.clone(),
        provider.id.clone(),
        native.session_id,
        false,
        timeout as u64,
        timeout as u64,
        RectifierConfig {
            enabled: false,
            ..Default::default()
        },
        OptimizerConfig::default(),
        CopilotOptimizerConfig {
            enabled: false,
            warmup_downgrade: false,
            ..Default::default()
        },
        0,
    )
    .stack_request(true);
    let mut probe_provider = provider.clone();
    if let Some(meta) = probe_provider.meta.as_mut() {
        // Native template probes own their prompt and body. Provider body
        // overrides may contain tools, input or saved response ids intended for
        // conversations; keep only header overrides (including authentication).
        if let Some(overrides) = meta.local_proxy_request_overrides.as_mut() {
            overrides.body = None;
        }
        meta.max_output_tokens = None;
    }
    let mut extensions = Extensions::new();
    extensions.insert(NativeClientRequest);
    extensions.insert(response_meta.clone());
    let response = forwarder
        .forward_keep_alive(
            app,
            &probe_provider,
            &native.endpoint,
            &native.body,
            &native.headers,
            &extensions,
        )
        .await
        .map_err(ProbeError::from_proxy)?;
    let http_status = response.status().as_u16();
    let encoding = get_content_encoding(response.headers());
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    super::response::validate_response(http_status, encoding, is_sse, response.bytes_stream())
        .await
        .map_err(|error| {
            let mut result = ProbeError::new(error.code, error.http_status);
            result.detail = error.body;
            result
        })
}
