//! Native response completion and streaming content decoding.

use crate::proxy::sse::{strip_sse_field, SseBlocks};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio_util::io::StreamReader;

const MAX_PROBE_BODY_BYTES: usize = 2 * 1024 * 1024;

pub(super) struct ProbeError {
    pub code: &'static str,
    pub http_status: Option<u16>,
    pub body: Option<String>,
}
impl ProbeError {
    fn new(code: &'static str, http_status: Option<u16>) -> Self {
        Self { code, http_status, body: None }
    }

    fn with_body(mut self, body: &str) -> Self {
        let mut end = body.len().min(crate::proxy::upstream_errors::MAX_ERROR_CAPTURE_BYTES);
        while !body.is_char_boundary(end) { end -= 1; }
        self.body = Some(body[..end].to_string());
        self
    }
}

pub(super) async fn validate_response<S>(
    status: u16,
    encoding: Option<String>,
    is_sse: bool,
    stream: S,
) -> Result<u16, ProbeError>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let raw_size = Arc::new(AtomicUsize::new(0));
    let counted_size = raw_size.clone();
    let stream = stream.map(move |chunk| {
        let chunk = chunk?;
        if counted_size.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len()
            > MAX_PROBE_BODY_BYTES
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "response_too_large",
            ));
        }
        Ok(chunk)
    });
    let buffered = BufReader::new(StreamReader::new(Box::pin(stream)));
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
    let mut reader: Pin<Box<dyn AsyncRead + Send>> = Box::pin(buffered);
    for coding in encoding
        .as_deref()
        .unwrap_or("")
        .split(',')
        .rev()
        .map(str::trim)
    {
        reader = match coding {
            "" | "identity" => reader,
            "gzip" | "x-gzip" => {
                let mut decoder = GzipDecoder::new(BufReader::new(reader));
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            "deflate" => Box::pin(ZlibDecoder::new(BufReader::new(reader))),
            "br" => Box::pin(BrotliDecoder::new(BufReader::new(reader))),
            "zstd" | "zst" => Box::pin(ZstdDecoder::new(BufReader::new(reader))),
            _ => return Err(ProbeError::new("invalid_response_encoding", Some(status))),
        };
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut processed = 0;
    loop {
        let length = reader.read(&mut buffer).await.map_err(|_| {
            ProbeError::new(
                if raw_size.load(Ordering::Relaxed) > MAX_PROBE_BODY_BYTES {
                    "response_too_large"
                } else if encoding.is_some() {
                    "invalid_response_encoding"
                } else {
                    "request_failed"
                },
                Some(status),
            )
        })?;
        if length == 0 {
            break;
        }
        if bytes.len() + length > MAX_PROBE_BODY_BYTES {
            return Err(ProbeError::new("response_too_large", Some(status)));
        }
        bytes.extend_from_slice(&buffer[..length]);
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or("")
            }
            Err(_) => return Err(ProbeError::new("invalid_response", Some(status))),
        };
        if is_sse
            || text.trim_start().starts_with("data:")
            || text.trim_start().starts_with("event:")
            || text.trim_start().starts_with(':')
        {
            let mut blocks = SseBlocks::new(&text[processed..]);
            let mut terminal = false;
            for block in blocks.by_ref() {
                terminal |= terminal_sse_block(block);
            }
            processed = text.len() - blocks.remainder().len();
            if terminal {
                validate_sse(&text[..processed])
                    .map_err(|code| ProbeError::new(code, Some(status)).with_body(&text[..processed]))?;
                // Match native clients: the protocol completion event ends the
                // request even when the HTTP connection remains open afterward.
                return Ok(status);
            }
        }
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ProbeError::new("invalid_response", Some(status)))?;
    if is_sse
        || text.trim_start().starts_with("data:")
        || text.trim_start().starts_with("event:")
        || text.trim_start().starts_with(':')
    {
        validate_sse(text).map_err(|code| ProbeError::new(code, Some(status)).with_body(text))?;
    } else {
        let value: Value = serde_json::from_str(text)
            .map_err(|_| ProbeError::new("invalid_response", Some(status)).with_body(text))?;
        if !valid_json_completion(&value) {
            return Err(ProbeError::new("incomplete_response", Some(status)).with_body(text));
        }
    }
    Ok(status)
}

fn terminal_sse_block(block: &str) -> bool {
    let mut event = "";
    let mut data = String::new();
    for line in block.lines() {
        if let Some(value) = strip_sse_field(line, "event") {
            event = value;
        }
        if let Some(value) = strip_sse_field(line, "data") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value);
        }
    }
    if matches!(
        event,
        "message_stop"
            | "response.completed"
            | "response.failed"
            | "response.incomplete"
            | "response.cancelled"
            | "error"
    ) {
        return true;
    }
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return false;
    };
    matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "message_stop"
                | "response.completed"
                | "response.failed"
                | "response.incomplete"
                | "response.cancelled"
                | "error"
        )
    ) || value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .get("finish_reason")
                    .is_some_and(|value| !value.is_null())
            })
        })
        || value
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates
                    .iter()
                    .any(|candidate| candidate.get("finishReason").is_some())
            })
}

fn has_error(value: &Value) -> bool {
    value.get("error").is_some_and(|value| !value.is_null())
        || matches!(
            value.get("type").and_then(Value::as_str),
            Some("error" | "response.failed" | "response.incomplete" | "response.cancelled")
        )
        || matches!(
            value.get("status").and_then(Value::as_str),
            Some("failed" | "incomplete" | "cancelled")
        )
}

fn has_text(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => items.iter().any(has_text),
        Value::Object(object) => {
            object.get("text").is_some_and(has_text) || object.get("content").is_some_and(has_text)
        }
        _ => false,
    }
}

fn response_has_text(value: &Value) -> bool {
    value
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output
                .iter()
                .any(|item| item.get("content").is_some_and(has_text))
        })
        || value.get("output_text").is_some_and(has_text)
}

fn valid_json_completion(value: &Value) -> bool {
    if has_error(value) {
        return false;
    }
    if value.get("status").and_then(Value::as_str) == Some("completed") {
        return response_has_text(value);
    }
    if matches!(
        value.get("stop_reason").and_then(Value::as_str),
        Some("end_turn" | "stop_sequence")
    ) {
        return value.get("content").is_some_and(has_text);
    }
    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        return candidates.iter().any(|candidate| {
            candidate.get("finishReason").and_then(Value::as_str) == Some("STOP")
                && candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part.get("text").is_some_and(has_text))
                    })
        });
    }
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice.get("finish_reason").and_then(Value::as_str) == Some("stop")
                    && choice.pointer("/message/content").is_some_and(has_text)
            })
        })
}

/// Only complete, productive responses count. A truncated stream, error after
/// output, heartbeat-only body, or max-token termination returns to acquisition.
fn validate_sse(text: &str) -> Result<(), &'static str> {
    let mut productive = false;
    let mut completed = false;
    let mut anthropic_stopped = false;
    let mut blocks = SseBlocks::new(text);
    let mut consume = |block: &str| -> Result<(), &'static str> {
        let mut event = "";
        let mut data = String::new();
        for line in block.lines() {
            if let Some(name) = strip_sse_field(line, "event") {
                event = name;
            }
            if let Some(part) = strip_sse_field(line, "data") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(part);
            }
        }
        if matches!(
            event,
            "error" | "response.failed" | "response.incomplete" | "response.cancelled"
        ) {
            return Err("stream_error");
        }
        if data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| "invalid_response")?;
        if has_error(&value) || value.get("response").is_some_and(has_error) {
            return Err("stream_error");
        }
        match value.get("type").and_then(Value::as_str).unwrap_or(event) {
            "response.output_text.delta" => productive |= value.get("delta").is_some_and(has_text),
            "response.completed" => {
                let response = value.get("response").ok_or("invalid_response")?;
                if response.get("status").and_then(Value::as_str) != Some("completed") {
                    return Err("incomplete_response");
                }
                productive |= response_has_text(response);
                completed = true;
            }
            "content_block_start" => {
                productive |= value.pointer("/content_block/text").is_some_and(has_text)
            }
            "content_block_delta" => {
                if value.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta") {
                    productive |= value.pointer("/delta/text").is_some_and(has_text);
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    if !matches!(reason, "end_turn" | "stop_sequence") {
                        return Err("incomplete_response");
                    }
                    anthropic_stopped = true;
                }
            }
            "message_stop" => {
                if !anthropic_stopped {
                    return Err("incomplete_response");
                }
                completed = true;
            }
            _ => {}
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                productive |= choice.pointer("/delta/content").is_some_and(has_text);
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    if reason != "stop" {
                        return Err("incomplete_response");
                    }
                    completed = true;
                }
            }
        }
        // Gemini Native is a supported Claude upstream format. It carries one
        // candidate plus finishReason rather than Anthropic message_stop.
        if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
            for candidate in candidates {
                productive |= candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part.get("text").is_some_and(has_text))
                    });
                if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                    if reason != "STOP" {
                        return Err("incomplete_response");
                    }
                    completed = true;
                }
            }
        }
        Ok(())
    };
    for block in blocks.by_ref() {
        consume(block)?;
    }
    consume(blocks.remainder())?;
    if productive && completed {
        Ok(())
    } else {
        Err("incomplete_response")
    }
}
