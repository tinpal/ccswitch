use serde_json::{json, Map, Value};

pub(super) const PROMPT: &str = "Reply only OK.";

/// Keep transport/model settings and short native client identity lines. Drop
/// instructions, tools and conversation before caching and after upstream
/// format conversion.
pub(in crate::proxy) fn minimize_prompt(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let responses = object.contains_key("input");
    let gemini = object.contains_key("contents");
    let anthropic = object.contains_key("system");
    let identity = [
        client_identity(object, claude_identity_line),
        client_identity(object, codex_identity_line),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");

    for field in [
        "instructions",
        "system",
        "systemInstruction",
        "system_instruction",
        "input",
        "messages",
        "contents",
        "tools",
        "functions",
        "tool_choice",
        "function_call",
        "parallel_tool_calls",
        "toolConfig",
        "tool_config",
        "mcp_servers",
        "previous_response_id",
        "conversation",
        "prompt",
        "context_management",
        "container",
        "cache_control",
        "cachedContent",
        "cached_content",
    ] {
        object.remove(field);
    }

    if responses {
        // Responses keeps the short native identity and required empty tool
        // fields, without the client's remaining built-in instructions.
        object.insert("instructions".into(), json!(identity));
        object.insert("tools".into(), json!([]));
        object.insert("parallel_tool_calls".into(), json!(false));
        object.insert(
            "input".into(),
            json!([{
                "role": "user",
                "content": [{"type": "input_text", "text": PROMPT}]
            }]),
        );
    } else if gemini {
        if !identity.is_empty() {
            object.insert(
                "systemInstruction".into(),
                json!({"parts": [{"text": identity}]}),
            );
        }
        object.insert(
            "contents".into(),
            json!([{"role": "user", "parts": [{"text": PROMPT}]}]),
        );
    } else {
        let mut messages = Vec::new();
        if !identity.is_empty() {
            // Retain only the installed client's identity text, without its
            // billing block, cache attributes or other system instructions.
            if anthropic {
                object.insert(
                    "system".into(),
                    json!([{"type": "text", "text": identity}]),
                );
            } else {
                messages.push(json!({"role": "system", "content": identity}));
            }
        }
        // A single text user message is valid for both Anthropic Messages and
        // OpenAI Chat Completions, including Claude's converted requests.
        messages.push(json!({"role": "user", "content": PROMPT}));
        object.insert("messages".into(), Value::Array(messages));
    }
}

fn client_identity(
    object: &Map<String, Value>,
    identify: fn(&str) -> Option<String>,
) -> Option<String> {
    ["instructions", "system", "systemInstruction", "system_instruction"]
        .into_iter()
        .filter_map(|field| object.get(field))
        .find_map(|value| identity_in_value(value, identify))
        .or_else(|| {
            ["messages", "input"]
                .into_iter()
                .filter_map(|field| object.get(field).and_then(Value::as_array))
                .flatten()
                .filter(|message| {
                    matches!(
                        message.get("role").and_then(Value::as_str),
                        Some("system" | "developer")
                    )
                })
                .find_map(|message| {
                    message
                        .get("content")
                        .and_then(|value| identity_in_value(value, identify))
                })
        })
}

fn identity_in_value(value: &Value, identify: fn(&str) -> Option<String>) -> Option<String> {
    match value {
        Value::String(text) => identify(text),
        Value::Array(values) => values
            .iter()
            .find_map(|value| identity_in_value(value, identify)),
        Value::Object(object) => ["text", "content", "parts"]
            .into_iter()
            .filter_map(|field| object.get(field))
            .find_map(|value| identity_in_value(value, identify)),
        _ => None,
    }
}

fn claude_identity_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| {
            matches!(
                *line,
                "You are a Claude agent, built on Anthropic's Claude Agent SDK."
                    | "You are Claude Code, Anthropic's official CLI for Claude."
            )
        })
        .map(str::to_owned)
}

fn codex_identity_line(text: &str) -> Option<String> {
    text.lines().map(str::trim).find_map(|line| {
        if line != "You are Codex." && !line.starts_with("You are Codex, ") {
            return None;
        }
        // Keep the native model identity sentence, not the workspace/personality
        // paragraph that follows it. A model version's decimal is not a boundary.
        let end = line.find(". ").map_or(line.len(), |index| index + 1);
        let sentence = &line[..end];
        (sentence.ends_with('.') && sentence.chars().count() <= 256)
            .then(|| sentence.to_owned())
    })
}
