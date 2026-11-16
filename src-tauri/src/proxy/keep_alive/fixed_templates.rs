//! Small client-compatible templates for machines without the local client.
//! Authentication is supplied by the provider forwarder, never by a template.

use http::{header, HeaderMap, HeaderValue};
use serde_json::{json, Value};

use crate::proxy::providers::codex_oauth_auth::{
    CODEX_OAUTH_CLIENT_VERSION, CODEX_OAUTH_ORIGINATOR,
};

use super::{
    client_templates::ClientKind,
    prompt::{minimize_prompt, PROMPT},
};

// RequestTemplate::fresh replaces this placeholder before every dispatch.
const SESSION_ID: &str = "00000000-0000-0000-0000-000000000000";
const CODEX_SYSTEM: &str = "You are Codex.\n\nThis is a standalone background model keep-alive request. Follow the user's instruction, respond in plain text, and finish the turn.";
const CLAUDE_SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.\n\nThis is a standalone background model keep-alive request. Follow the user's instruction, respond in plain text, and finish the turn.";

pub(super) fn build(
    client: ClientKind,
    model: &str,
    use_minimal_prompt: bool,
) -> (&'static str, HeaderMap, Value) {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (endpoint, mut body) = match client {
        ClientKind::Codex => {
            headers.insert(
                header::ACCEPT,
                HeaderValue::from_static("text/event-stream"),
            );
            headers.insert(
                header::USER_AGENT,
                HeaderValue::from_str(&format!(
                    "{CODEX_OAUTH_ORIGINATOR}/{CODEX_OAUTH_CLIENT_VERSION}"
                ))
                .expect("built-in Codex User-Agent is valid"),
            );
            headers.insert(
                "originator",
                HeaderValue::from_static(CODEX_OAUTH_ORIGINATOR),
            );
            headers.insert(
                "version",
                HeaderValue::from_static(CODEX_OAUTH_CLIENT_VERSION),
            );
            headers.insert("session_id", HeaderValue::from_static(SESSION_ID));
            (
                "/responses",
                json!({
                    "model": model,
                    "instructions": CODEX_SYSTEM,
                    "input": [{
                        "role": "user",
                        "content": [{"type": "input_text", "text": PROMPT}]
                    }],
                    "tools": [],
                    "parallel_tool_calls": false,
                    "stream": true,
                    "store": false,
                    "prompt_cache_key": SESSION_ID
                }),
            )
        }
        ClientKind::Claude => {
            headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
            headers.insert(
                header::USER_AGENT,
                HeaderValue::from_static("claude-cli/2.1.325 (external, sdk-cli)"),
            );
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            headers.insert("x-app", HeaderValue::from_static("cli"));
            (
                "/v1/messages",
                json!({
                    "model": model,
                    "system": [{"type": "text", "text": CLAUDE_SYSTEM}],
                    "messages": [{
                        "role": "user",
                        "content": [{"type": "text", "text": PROMPT}]
                    }],
                    "tools": [],
                    "metadata": {
                        "user_id": json!({"session_id": SESSION_ID}).to_string()
                    },
                    "max_tokens": 64,
                    "stream": true
                }),
            )
        }
    };
    if use_minimal_prompt {
        minimize_prompt(&mut body);
    }
    (endpoint, headers, body)
}
