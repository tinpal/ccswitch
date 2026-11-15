//! Bounded, redacted diagnostic records for background probes and retry attempts.

use crate::database::{Database, UpstreamErrorLog, UpstreamErrorSource};
use crate::provider::Provider;
use super::{content_encoding, hyper_client::ProxyResponse, ProxyError};
use bytes::Bytes;
use futures::StreamExt;
use http::HeaderMap;
use regex::Regex;
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

pub(super) const MAX_ERROR_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_STORED_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone, Default)]
pub(super) struct UpstreamResponseMeta(Arc<Mutex<Option<String>>>);

impl UpstreamResponseMeta {
    pub(super) fn capture(&self, headers: &HeaderMap) {
        if let Ok(mut slot) = self.0.lock() { *slot = response_request_id(headers); }
    }

    pub(super) fn request_id(&self) -> Option<String> {
        self.0.lock().ok().and_then(|slot| slot.clone())
    }
}

fn response_request_id(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id", "x-ms-request-id", "anthropic-request-id"]
        .into_iter().find_map(|key| headers.get(key)?.to_str().ok().map(str::to_string))
}

pub(super) struct UpstreamErrorContext {
    db: Arc<Database>,
    source: UpstreamErrorSource,
    app_type: String,
    provider_id: String,
    provider_name: String,
    model: String,
    request_id: String,
    secrets: Vec<String>,
}

impl UpstreamErrorContext {
    pub(super) fn new(
        db: Arc<Database>, source: UpstreamErrorSource, app_type: &str,
        provider: &Provider, model: &str, request_id: String,
    ) -> Self {
        let mut secrets = Vec::new();
        collect_secrets(&provider.settings_config, &mut secrets);
        if let Ok(meta) = serde_json::to_value(&provider.meta) { collect_secrets(&meta, &mut secrets); }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Self { db, source, app_type: app_type.into(), provider_id: provider.id.clone(),
            provider_name: provider.name.clone(), model: model.into(), request_id, secrets }
    }

    pub(super) fn with_headers(mut self, headers: &HeaderMap) -> Self {
        for (name, value) in headers {
            if sensitive_key(name.as_str()) {
                if let Ok(value) = value.to_str() { add_secret(value, &mut self.secrets); }
            }
        }
        self.secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        self.secrets.dedup();
        self
    }

    pub(super) async fn record(
        &self, status: Option<u16>, fallback_code: &str, body: Option<&str>,
        upstream_request_id: Option<String>, attempt: u64, elapsed: Duration,
    ) {
        let details = error_details(body, fallback_code, &self.secrets);
        let entry = UpstreamErrorLog {
            id: 0, created_at: chrono::Utc::now().timestamp_millis(), source: self.source,
            app_type: self.app_type.clone(), provider_id: self.provider_id.clone(),
            provider_name: sanitize_text(&self.provider_name, &self.secrets),
            model: sanitize_text(&self.model, &self.secrets), request_id: self.request_id.clone(),
            upstream_request_id: upstream_request_id.or(details.request_id)
                .map(|id| clip(&sanitize_text(&id, &self.secrets), 256).0),
            attempt, status_code: status, error_code: details.code, message: details.message,
            response_body: details.body, body_truncated: details.truncated,
            elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
        };
        let db = Arc::clone(&self.db);
        // Await a blocking worker: database contention never blocks the async executor,
        // and zero-delay retry lanes cannot accumulate unbounded detached log jobs.
        match tokio::task::spawn_blocking(move || db.insert_upstream_error(&entry)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("[Upstream Errors] Could not store record: {error}"),
            Err(error) => log::warn!("[Upstream Errors] Record worker failed: {error}"),
        }
    }

    pub(super) async fn record_proxy(
        &self, error: &ProxyError, headers: Option<&HeaderMap>, attempt: u64, elapsed: Duration,
    ) {
        let (status, code, body) = match error {
            ProxyError::UpstreamError { status, body } => (Some(*status), "upstream_http_error", body.clone()),
            ProxyError::Timeout(_) | ProxyError::StreamIdleTimeout(_) => (None, "timeout", Some(error.to_string())),
            ProxyError::AuthError(_) => (None, "authentication_error", Some(error.to_string())),
            _ => (None, "request_failed", Some(error.to_string())),
        };
        self.record(status, code, body.as_deref(), headers.and_then(response_request_id), attempt, elapsed).await;
    }

    pub(super) async fn record_response(
        &self, response: ProxyResponse, code: &str, attempt: u64, elapsed: Duration,
    ) -> ProxyResponse {
        let status = response.status();
        let headers = response.headers().clone();
        let (response, raw, incomplete) = capture_response(response).await;
        let encoding = content_encoding::get_content_encoding(&headers);
        let body = if let Some(encoding) = encoding {
            content_encoding::decompress_body_with_limit(&encoding, &raw, MAX_ERROR_CAPTURE_BYTES)
                .ok().flatten().and_then(|bytes| String::from_utf8(bytes).ok())
        } else {
            Some(String::from_utf8_lossy(&raw).into_owned())
        };
        let body = body.map(|body| if incomplete { format!("{body}\n[response capture truncated]") } else { body });
        self.record(Some(status.as_u16()), code, body.as_deref(), response_request_id(&headers), attempt, elapsed).await;
        response
    }
}

/// Capture a bounded diagnostic copy while replaying every original byte/error.
/// Non-retryable responses therefore keep their original body and Content-Length.
async fn capture_response(response: ProxyResponse) -> (ProxyResponse, Vec<u8>, bool) {
    if let ProxyResponse::Buffered { ref body, .. } = response {
        let incomplete = body.len() > MAX_ERROR_CAPTURE_BYTES;
        let raw = body[..body.len().min(MAX_ERROR_CAPTURE_BYTES)].to_vec();
        return (response, raw, incomplete);
    }
    let status = response.status();
    let headers = response.headers().clone();
    let mut stream = Box::pin(response.bytes_stream());
    let mut replay = Vec::new();
    let mut raw = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    let incomplete = loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                let room = MAX_ERROR_CAPTURE_BYTES.saturating_sub(raw.len());
                raw.extend_from_slice(&chunk[..chunk.len().min(room)]);
                replay.push(Ok::<Bytes, std::io::Error>(chunk));
                if raw.len() >= MAX_ERROR_CAPTURE_BYTES { break true; }
            }
            Ok(Some(Err(error))) => { replay.push(Err(error)); break true; }
            Ok(None) => break false,
            Err(_) => break true,
        }
    };
    let response = ProxyResponse::streamed(status, headers, futures::stream::iter(replay).chain(stream));
    (response, raw, incomplete)
}

fn clip(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit { return (text.into(), false); }
    let mut boundary = limit;
    while !text.is_char_boundary(boundary) { boundary -= 1; }
    (text[..boundary].into(), true)
}

fn sensitive_key(key: &str) -> bool {
    let key: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect();
    key.contains("apikey") || key.contains("authtoken") || key.contains("authorization")
        || key.contains("accesstoken") || key.contains("refreshtoken") || key.contains("idtoken")
        || matches!(key.as_str(), "key" | "token" | "secret" | "password" | "cookie" | "setcookie")
}

fn add_secret(value: &str, secrets: &mut Vec<String>) {
    let value = value.trim();
    if value.len() >= 4 { secrets.push(value.into()); }
    if let Some((scheme, token)) = value.split_once(' ') {
        if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic") {
            if token.trim().len() >= 4 { secrets.push(token.trim().into()); }
        }
    }
}

fn collect_secrets(value: &Value, secrets: &mut Vec<String>) {
    match value {
        Value::Object(object) => for (key, value) in object {
            if sensitive_key(key) { if let Some(value) = value.as_str() { add_secret(value, secrets); } }
            collect_secrets(value, secrets);
        },
        Value::Array(values) => for value in values { collect_secrets(value, secrets); },
        _ => {}
    }
}

fn sanitize_text(text: &str, secrets: &[String]) -> String {
    static BEARER: OnceLock<Regex> = OnceLock::new();
    static KEYS: OnceLock<Regex> = OnceLock::new();
    static ASSIGNMENT: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    let mut text = text.to_string();
    for secret in secrets { text = text.replace(secret, "[redacted]"); }
    text = BEARER.get_or_init(|| Regex::new(r"(?i)\b(?:bearer|basic)\s+[A-Za-z0-9._~+/=\-]+").unwrap())
        .replace_all(&text, "[redacted authorization]").into_owned();
    text = KEYS.get_or_init(|| Regex::new(r"\b(?:sk-[A-Za-z0-9_-]+|eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+)").unwrap())
        .replace_all(&text, "[redacted]").into_owned();
    text = ASSIGNMENT.get_or_init(|| Regex::new(r#"(?i)((?:api[_-]?key|access[_-]?token|refresh[_-]?token|authorization|password|secret|cookie)\s*["']?\s*[:=]\s*["']?)[^\s"',;&<>]+"#).unwrap())
        .replace_all(&text, "${1}[redacted]").into_owned();
    URL.get_or_init(|| Regex::new(r#"https?://[^\s"'<>]+"#).unwrap()).replace_all(&text, |captures: &regex::Captures<'_>| {
        match url::Url::parse(&captures[0]) {
            Ok(mut url) => {
                let _ = url.set_username(""); let _ = url.set_password(None);
                url.set_query(None); url.set_fragment(None); url.to_string()
            }
            Err(_) => "[redacted URL]".into(),
        }
    }).into_owned()
}

fn sanitize_json(value: &mut Value, secrets: &[String]) {
    match value {
        Value::Object(object) => for (key, value) in object {
            if sensitive_key(key) || matches!(key.as_str(), "input" | "output" | "messages" | "content" | "instructions" | "system" | "prompt" | "encrypted_content") {
                *value = Value::String("[redacted]".into());
            } else { sanitize_json(value, secrets); }
        },
        Value::Array(values) => for value in values { sanitize_json(value, secrets); },
        Value::String(text) => *text = sanitize_text(text, secrets),
        _ => {}
    }
}

struct ErrorDetails { code: String, message: String, body: Option<String>, truncated: bool, request_id: Option<String> }

fn error_value(value: &Value) -> &Value {
    value.pointer("/response/error").filter(|value| !value.is_null())
        .or_else(|| value.get("error").filter(|value| !value.is_null())).unwrap_or(value)
}

fn error_details(body: Option<&str>, fallback: &str, secrets: &[String]) -> ErrorDetails {
    let body = body.filter(|body| !body.trim().is_empty());
    let parsed = body.and_then(|body| serde_json::from_str::<Value>(body).ok()).or_else(|| {
        body.and_then(|body| body.lines().filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .filter(|value| value.get("error").is_some() || value.pointer("/response/error").is_some()
                || matches!(value.get("type").and_then(Value::as_str), Some("error" | "response.failed" | "response.incomplete")))
            .last())
    });
    let error = parsed.as_ref().map(error_value);
    let code = error.and_then(|value| value.get("code").and_then(Value::as_str)
        .or_else(|| value.get("type").and_then(Value::as_str))).filter(|s| !s.is_empty()).unwrap_or(fallback);
    let message = error.and_then(|value| value.get("message").and_then(Value::as_str)
        .or_else(|| value.get("detail").and_then(Value::as_str)).or_else(|| value.as_str()))
        .or_else(|| if parsed.is_none() { body } else { None }).unwrap_or(fallback);
    let request_id = parsed.as_ref().and_then(|value| value.get("request_id").or_else(|| error_value(value).get("request_id")))
        .and_then(Value::as_str).map(str::to_string);
    let code = clip(&sanitize_text(code, secrets), 128).0;
    let message = clip(&sanitize_text(message, secrets), 1024).0;
    let sanitized = if let Some(mut value) = parsed {
        // An error event may include a full response object. Keep only diagnostic
        // metadata; successful output and echoed prompts have no place in this log.
        sanitize_json(&mut value, secrets);
        Some(serde_json::to_string_pretty(&value).unwrap_or_default())
    } else { body.map(|body| sanitize_text(body, secrets)) };
    let (body, truncated) = sanitized.map(|body| {
        let (body, clipped) = clip(&body, MAX_STORED_BODY_BYTES);
        let truncated = clipped || body.contains("[response capture truncated]");
        (Some(body), truncated)
    }).unwrap_or((None, false));
    ErrorDetails { code, message, body, truncated, request_id }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upstream_errors_preserve_null_code_message_and_redact_credentials() {
        let body = json!({"error":{"code":null,"type":"gateway_error","message":"No channel for private-token-123; Bearer new-token; https://user:password@example.com/v1?key=secret","api_key":"unknown-key"},"input":"private prompt"}).to_string();
        let details = error_details(Some(&body), "http_500", &["private-token-123".into()]);
        assert_eq!(details.code, "gateway_error");
        assert!(details.message.contains("No channel"));
        let body = details.body.unwrap();
        for secret in ["private-token-123", "new-token", "user:password", "key=secret", "unknown-key", "private prompt"] { assert!(!body.contains(secret), "leaked {secret}"); }
    }

    #[test]
    fn upstream_errors_extract_sse_failure_and_bound_unicode_details() {
        let body = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"get_channel_failed\",\"message\":\"模型满载\"},\"output\":[\"secret answer\"]}}\n\n";
        let details = error_details(Some(body), "http_200", &[]);
        assert_eq!(details.code, "get_channel_failed");
        assert_eq!(details.message, "模型满载");
        assert!(!details.body.unwrap().contains("secret answer"));
        let details = error_details(Some(&"错".repeat(20_000)), "http_500", &[]);
        assert!(details.truncated);
        assert!(details.body.unwrap().len() <= MAX_STORED_BODY_BYTES);
    }

    #[tokio::test]
    async fn upstream_errors_capture_preserves_large_streamed_response() {
        let body = Bytes::from(vec![b'x'; MAX_ERROR_CAPTURE_BYTES * 2]);
        let response = ProxyResponse::streamed(http::StatusCode::BAD_REQUEST, HeaderMap::new(),
            futures::stream::iter(vec![Ok(body.clone()), Ok(Bytes::from_static(b"tail"))]));
        let (response, captured, truncated) = capture_response(response).await;
        assert!(truncated);
        assert_eq!(captured.len(), MAX_ERROR_CAPTURE_BYTES);
        let replay = response.bytes_with_limit(MAX_ERROR_CAPTURE_BYTES * 3).await.unwrap();
        assert_eq!(&replay[..body.len()], &body[..]);
        assert_eq!(&replay[body.len()..], b"tail");
    }
}
