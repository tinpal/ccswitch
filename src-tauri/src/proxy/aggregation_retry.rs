//! Same-request retries before response output begins.
//!
//! The transport closure captures the resolved URL and headers. Each attempt receives
//! the current serialized body, so rectification cannot be undone by ordinary retries.
//! It is awaited inline: dropping the client handler also drops the current send, without
//! spawning a background retry task. Native Responses JSON/SSE failures are inspected
//! only before productive output, including temporary server failures. Once output
//! begins, the live response is delivered without buffering or retrying later errors.
//! Accepted resource-conflict repairs also teach a bounded session/upstream policy,
//! applied before later requests send their first attempt. Retry budgets stay local.

use super::{
    hyper_client::ProxyResponse,
    opaque_state_rectifier::{
        detect_opaque_state_rejection, in_stream_rejection, rectify_opaque_state,
        OpaqueStateRectifyResult, OpaqueStateRejection,
    },
    types::{AggregationRetryConfig, RectifierConfig},
    ProxyError,
};
use bytes::{Bytes, BytesMut};
use futures::{stream::FuturesUnordered, Stream, StreamExt};
use serde_json::Value;
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

/// Keep one bounded error body for returning the last real upstream error on expiry.
const MAX_RETRY_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Lifecycle events can precede a rate limit failure. Keep a bounded prefix to replay.
const MAX_RESPONSE_PREFIX_BYTES: usize = 256 * 1024;

mod opaque_memory;
pub(super) use opaque_memory::OpaqueStateScope;

#[cfg(test)]
mod tests;

#[derive(Default)]
struct RetryState {
    started: OnceLock<Instant>,
    retries: u32,
    has_sent: bool,
    consecutive_failures: u32,
    last_error: Option<RetainedUpstreamError>,
    opaque: HashMap<(String, Option<OpaqueStateScope>), OpaqueRetryState>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PendingRetry {
    due: Instant,
    consecutive_failures: u32,
    delay: Duration,
}

impl PendingRetry {
    fn after_failure(config: &AggregationRetryConfig, consecutive_failures: u32) -> Self {
        let delay = config.retry_delay.delay(config.retry_interval_seconds, consecutive_failures);
        Self { due: Instant::now() + delay, consecutive_failures, delay }
    }
}

struct RetainedUpstreamError {
    status: u16,
    body: Option<String>,
}

impl RetainedUpstreamError {
    fn remember(slot: &mut Option<Self>, error: &ProxyError) {
        if let ProxyError::UpstreamError { status, body } = error {
            if body.as_ref().is_some_and(|body| !body.trim().is_empty()) || slot.is_none() {
                *slot = Some(Self {
                    status: *status,
                    body: body.clone(),
                });
            }
        }
    }

    fn to_error(&self) -> ProxyError {
        ProxyError::UpstreamError {
            status: self.status,
            body: self.body.clone(),
        }
    }
}

#[derive(Default)]
struct OpaqueRetryState {
    rejection: Option<OpaqueStateRejection>,
    rectified: bool,
    failure_logged: bool,
    success_logged: bool,
}

#[derive(Clone, Copy)]
pub(super) struct AggregationRequest<'a> {
    pub app_type: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub native_responses: bool,
    pub inspect_stream_start: bool,
    pub body: &'a Value,
    pub rectifier: &'a RectifierConfig,
    pub codex_third_party: bool,
    pub opaque_scope: Option<OpaqueStateScope>,
    pub error_log: Option<&'a super::upstream_errors::UpstreamErrorContext>,
}

#[derive(Clone)]
struct PreparedRequest<'a> {
    context: AggregationRequest<'a>,
    body: Cow<'a, Value>,
    bytes: Bytes,
}

impl<'a> PreparedRequest<'a> {
    fn new(context: AggregationRequest<'a>, bytes: Bytes, state: &mut OpaqueRetryState) -> Self {
        let mut request = Self {
            context,
            body: Cow::Borrowed(context.body),
            bytes,
        };
        // Compatibility re-entry may prepare the original client body again.
        // Reapply all cleanup rules already learned for this provider, preserving
        // other compatibility edits without resurrecting old ciphertext or IDs.
        if context.app_type == "codex" && context.native_responses && context.rectifier.enabled {
            if let Some(rejection) = context.opaque_scope.and_then(OpaqueStateScope::load) {
                state.rejection = Some(match state.rejection {
                    Some(previous) => previous.merge(rejection),
                    None => rejection,
                });
            }
            if let Some(rejection) = state.rejection {
                let result = rectify_opaque_state(request.body.to_mut(), rejection);
                if result.applied {
                    state.rectified = true;
                    request.serialize();
                    log::info!(
                        "[Codex] [RECT-023] Provider={} Model={} Mode=preflight RemovedReasoning={} ReplacedEncryptedParts={} RemovedItemIds={} ReplacedCompaction={}",
                        context.provider, context.model, result.removed_reasoning_items,
                        result.replaced_encrypted_parts, result.removed_foreign_ids,
                        result.replaced_compaction_items,
                    );
                }
            }
        }
        request
    }

    fn serialize(&mut self) {
        self.bytes = Bytes::from(self.body.to_string());
    }

    fn detect(&self, error: &ProxyError) -> Option<OpaqueStateRejection> {
        if self.context.app_type != "codex" {
            return None;
        }
        self.context
            .native_responses
            .then(|| {
                detect_opaque_state_rejection(
                    error,
                    self.context.rectifier,
                    &self.body,
                    self.context.codex_third_party,
                )
            })
            .flatten()
    }

    fn stream_rejection(&self, error: &Value) -> Option<(ProxyError, OpaqueStateRejection)> {
        if self.context.app_type != "codex"
            || !self.context.native_responses
            || !self.context.rectifier.enabled
        {
            return None;
        }
        let error = in_stream_rejection(error, &self.body)?;
        let rejection = self.detect(&error)?;
        Some((error, rejection))
    }

    fn rectify(
        &mut self,
        state: &mut OpaqueRetryState,
        rejection: OpaqueStateRejection,
    ) -> OpaqueStateRectifyResult {
        // Every recognized rejection runs the existing idempotent rectifier, even
        // when a previous pass already removed everything it is allowed to remove.
        let result = rectify_opaque_state(self.body.to_mut(), rejection);
        state.rejection = Some(match state.rejection {
            Some(previous) => previous.merge(rejection),
            None => rejection,
        });
        state.failure_logged = false;
        state.success_logged = false;
        if result.applied {
            state.rectified = true;
            self.serialize();
        }
        result
    }

    fn log_rectification(
        &self,
        result: &OpaqueStateRectifyResult,
        retries: u32,
        elapsed: f64,
        delay_seconds: f64,
    ) {
        log::info!(
            "[Codex] [RECT-020] Provider={} Model={} Retry=opaque_state Applied={} Attempt={} Delay={delay_seconds:.3}s Elapsed={:.3}s RemovedReasoning={} ReplacedEncryptedParts={} RemovedItemIds={} ReplacedCompaction={}",
            self.context.provider, self.context.model, result.applied, retries, elapsed, result.removed_reasoning_items,
            result.replaced_encrypted_parts, result.removed_foreign_ids, result.replaced_compaction_items,
        );
    }

    fn log_outcome(&self, state: &mut OpaqueRetryState, successful: bool) {
        if state.rejection.is_none() {
            return;
        }
        if successful && !state.success_logged {
            state.success_logged = true;
            log::info!(
                "[Codex] [RECT-021] Provider={} Model={} Rectified request succeeded",
                self.context.provider,
                self.context.model
            );
        } else if !successful && !state.failure_logged {
            state.failure_logged = true;
            log::warn!("[Codex] [RECT-022] Provider={} Model={} Rectified attempt failed; applying aggregation retry policy", self.context.provider, self.context.model);
        }
    }
}

/// Retry accounting belongs to one client RequestContext, including compatibility retries.
/// Cleanup generations are isolated by provider/upstream identity. Accepted resource-conflict
/// repairs share only their policy with later requests from the same client session.
pub(super) struct AggregationRetry {
    config: AggregationRetryConfig,
    state: Mutex<RetryState>,
    exhausted: AtomicBool,
    successful: AtomicBool,
    request_id: String,
}

impl AggregationRetry {
    pub(super) fn new(config: AggregationRetryConfig) -> Self {
        Self {
            config,
            state: Mutex::new(RetryState::default()),
            exhausted: AtomicBool::new(false),
            successful: AtomicBool::new(false),
            request_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(super) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(super) fn was_exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Acquire)
    }

    pub(super) async fn log_success(&self, provider: &str, model: &str) {
        if self.was_exhausted() || !self.successful.load(Ordering::Acquire) {
            return;
        }
        let state = self.state.lock().await;
        if let Some(started) = state.started.get().filter(|_| state.retries > 0) {
            log::info!(
                "[Aggregation Retry] Success Provider={provider} Model={model} Attempts={} Retries={} RetryMode={} Elapsed={:.3}s",
                u64::from(state.retries) + 1, state.retries, self.config.retry_delay.mode.as_str(),
                started.elapsed().as_secs_f64()
            );
        }
    }

    pub(super) async fn send<F, Fut>(
        &self,
        context: AggregationRequest<'_>,
        bytes: Bytes,
        mut send: F,
    ) -> Result<ProxyResponse, ProxyError>
    where
        F: FnMut(Bytes) -> Fut,
        Fut: Future<Output = Result<ProxyResponse, ProxyError>>,
    {
        self.successful.store(false, Ordering::Release);
        if !self.config.enabled {
            return send(bytes).await;
        }
        let mut state = self.state.lock().await;
        let RetryState {
            started,
            retries,
            has_sent,
            consecutive_failures,
            last_error,
            opaque,
        } = &mut *state;
        let started: &OnceLock<Instant> = started;
        let opaque = opaque
            .entry((context.provider.to_string(), context.opaque_scope))
            .or_default();
        // Attempts share an immutable snapshot. A rectification creates the next
        // body generation once, rather than cloning the whole history per lane.
        let mut request = Arc::new(PreparedRequest::new(context, bytes, opaque));
        let AggregationRequest {
            app_type,
            provider,
            model,
            ..
        } = context;
        let budget = Duration::from_secs(self.config.max_elapsed_seconds.into());
        let limit = self
            .config
            .max_retries
            .map_or_else(|| "unlimited".to_string(), |n| n.to_string());
        let concurrency = if self.config.parallel_enabled {
            self.config.parallel_requests.clamp(1, 64) as usize
        } else {
            1
        };
        let retry_mode = self.config.retry_delay.mode.as_str();
        let mut active = FuturesUnordered::new();
        // Cooldowns are separate from active sends. A waiting lane neither blocks
        // another lane's response nor spends a retry slot before it is sent.
        let mut pending = BinaryHeap::new();
        if *has_sent {
            pending.extend((0..concurrency).map(|_| Reverse(PendingRetry::after_failure(
                &self.config, (*consecutive_failures).max(1),
            ))));
        } else {
            pending.push(Reverse(PendingRetry {
                due: Instant::now(), consecutive_failures: 0, delay: Duration::ZERO,
            }));
        }
        let mut last_response = None;
        let mut next_rectification = None;
        let mut error_code = "compatibility_retry";
        let mut error_status = last_error.as_ref().map_or(0, |error| error.status);
        let mut terminal = None;
        let mut exhausted_reason = None;

        // One scheduler owns every lane's budget, current body and winner. Dropping
        // this future drops all active sends; no retry task is detached from it.
        loop {
            while terminal.is_none() && active.len() < concurrency {
                let Some(Reverse(next)) = pending.peek().copied() else {
                    break;
                };
                let initial_attempt = !*has_sent;
                if !initial_attempt {
                    let start = *started.get_or_init(Instant::now);
                    let deadline = start + budget;
                    exhausted_reason = if self.was_exhausted() || Instant::now() >= deadline {
                        Some("retry budget exceeded")
                    } else if self
                        .config
                        .max_retries
                        .is_some_and(|limit| *retries >= limit)
                    {
                        Some("retry count exceeded")
                    } else {
                        None
                    };
                    if exhausted_reason.is_some() {
                        pending.clear();
                        break;
                    }
                    if Instant::now() < next.due {
                        break;
                    }
                    // Keep cancellation responsive even with immediately failing
                    // upstreams. Each launched lane spends one shared retry slot.
                    tokio::task::yield_now().await;
                    if Instant::now() >= deadline {
                        exhausted_reason = Some("retry budget exceeded");
                        pending.clear();
                        break;
                    }
                    *retries = retries.saturating_add(1);
                    if let Some(result) = next_rectification.take() {
                        request.log_rectification(
                            &result,
                            *retries,
                            start.elapsed().as_secs_f64(),
                            next.delay.as_secs_f64(),
                        );
                    } else {
                        log::info!(
                            "[Aggregation Retry] App={app_type} Provider={provider} Model={model} Mode=ordinary Status={error_status} ErrorCode={error_code} Attempt={}/{limit} ParallelRequests={concurrency} InFlight={} RetryMode={retry_mode} Delay={:.3}s Elapsed={:.3}s",
                            retries, active.len() + 1, next.delay.as_secs_f64(), start.elapsed().as_secs_f64()
                        );
                    }
                }
                pending.pop();
                *has_sent = true;
                active.push(run_attempt(
                    send(request.bytes.clone()),
                    Arc::clone(&request),
                    started,
                    budget,
                    u64::from(*retries) + 1,
                    next.consecutive_failures,
                ));
                // Parallelism starts only after the first request has failed.
                if initial_attempt {
                    break;
                }
            }

            let wake_at = pending
                .peek()
                .filter(|_| terminal.is_none() && active.len() < concurrency)
                .map(|Reverse(next)| {
                    started
                        .get()
                        .map_or(next.due, |start| next.due.min(*start + budget))
                });
            let outcome = if let Some(wake_at) = wake_at {
                tokio::select! {
                    biased;
                    outcome = active.next(), if !active.is_empty() => outcome,
                    _ = tokio::time::sleep_until(wake_at) => continue,
                }
            } else {
                active.next().await
            };
            let Some((lane_failures, outcome)) = outcome else {
                if let Some(result) = terminal {
                    return result;
                }
                return self.exhaust(
                    context,
                    exhausted_reason.unwrap_or("retry count exceeded"),
                    *started.get_or_init(Instant::now),
                    *retries,
                    last_failure(last_response, last_error.as_ref()),
                );
            };
            let outcome = match outcome {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    request.log_outcome(opaque, false);
                    log::info!(
                        "[Aggregation Retry] Stopped Provider={provider} Model={model} Reason=transport error RemainingInFlight={}",
                        active.len()
                    );
                    // Preserve the existing transport-error policy. Already sent
                    // siblings can still win, but no new attempts are launched.
                    if terminal.is_none() {
                        terminal = Some(Err(error));
                    }
                    pending.clear();
                    continue;
                }
                Err(_) => {
                    request.log_outcome(opaque, false);
                    active.clear();
                    return self.exhaust(
                        context,
                        "retry budget exceeded",
                        *started.get_or_init(Instant::now),
                        *retries,
                        terminal
                            .filter(|result| {
                                matches!(result, Ok(_) | Err(ProxyError::UpstreamError { .. }))
                            })
                            .unwrap_or_else(|| last_failure(last_response, last_error.as_ref())),
                    );
                }
            };
            let next_failures = lane_failures.saturating_add(1);
            if !matches!(&outcome, Attempt::Complete(_, true) | Attempt::PrimingTimedOut(_)) {
                *consecutive_failures = (*consecutive_failures).max(next_failures);
            }
            match outcome {
                Attempt::Complete(response, true) => {
                    *consecutive_failures = 0;
                    let cancelled = active.len();
                    active.clear();
                    request.log_outcome(opaque, true);
                    if opaque.rectified {
                        if let (Some(scope), Some(rejection)) =
                            (context.opaque_scope, opaque.rejection)
                        {
                            scope.remember(rejection);
                        }
                    }
                    self.successful.store(true, Ordering::Release);
                    if concurrency > 1 && *retries > 0 {
                        log::info!(
                            "[Aggregation Retry] Selected Provider={provider} Model={model} ParallelRequests={concurrency} Cancelled={cancelled} Retries={}",
                            retries
                        );
                    }
                    // inspect_attempt commits at the first productive event. The
                    // winner streams normally; later errors never restart a race.
                    return Ok(response);
                }
                Attempt::Complete(response, false) => {
                    request.log_outcome(opaque, false);
                    if *retries > 0 {
                        log::info!(
                            "[Aggregation Retry] Stopped Provider={provider} Model={model} Status={} Reason=non-retryable response RemainingInFlight={}",
                            response.status().as_u16(), active.len()
                        );
                    }
                    terminal = Some(Ok(response));
                    pending.clear();
                }
                Attempt::OpaqueRejected(response, error, rejection) => {
                    request.log_outcome(opaque, false);
                    RetainedUpstreamError::remember(last_error, &error);
                    let result = Arc::make_mut(&mut request).rectify(opaque, rejection);
                    if result.applied {
                        // All replacement lanes use this generation and wait from
                        // this rejection, including lanes still in a cooldown.
                        pending.clear();
                    }
                    if result.applied && !active.is_empty() {
                        // A sibling with the previous body must not deliver old
                        // ciphertext/IDs after this request's cleanup advances.
                        log::info!(
                            "[Aggregation Retry] Cancelled Provider={provider} Model={model} Reason=request rectified InFlight={}",
                            active.len()
                        );
                        active.clear();
                    }
                    next_rectification = Some(result);
                    error_code = "opaque_state";
                    error_status = response.status().as_u16();
                    started.get_or_init(Instant::now);
                    // The worker already buffered the bounded HTTP/SSE error.
                    last_response = Some(response);
                }
                Attempt::Retry(response, code) => {
                    request.log_outcome(opaque, false);
                    error_code = code;
                    error_status = response.status().as_u16();
                    started.get_or_init(Instant::now);
                    last_response = Some(response);
                }
                Attempt::HttpRetry(response, error, code) => {
                    request.log_outcome(opaque, false);
                    error_code = code;
                    error_status = response.status().as_u16();
                    started.get_or_init(Instant::now);
                    let meaningful = matches!(&error,
                        ProxyError::UpstreamError { body: Some(body), .. } if !body.trim().is_empty()
                    );
                    if meaningful || (last_response.is_none() && last_error.is_none()) {
                        last_response = Some(response);
                    }
                    RetainedUpstreamError::remember(last_error, &error);
                }
                Attempt::PrimingTimedOut(response) => {
                    active.clear();
                    if let Some(start) = started.get() {
                        request.log_outcome(opaque, false);
                        return self.exhaust(
                            context,
                            "retry budget exceeded",
                            *start,
                            *retries,
                            terminal
                                .filter(|result| {
                                    matches!(result, Ok(_) | Err(ProxyError::UpstreamError { .. }))
                                })
                                .unwrap_or_else(|| {
                                    last_failure(last_response, last_error.as_ref())
                                }),
                        );
                    }
                    // No failure occurred: preserve the original live stream.
                    return Ok(response);
                }
            }
            if terminal.is_none() {
                let missing = concurrency.saturating_sub(active.len() + pending.len());
                pending.extend((0..missing).map(|_| Reverse(PendingRetry::after_failure(
                    &self.config, next_failures,
                ))));
            }
        }
    }

    fn exhaust(
        &self,
        context: AggregationRequest<'_>,
        reason: &'static str,
        started: Instant,
        retries: u32,
        result: Result<ProxyResponse, ProxyError>,
    ) -> Result<ProxyResponse, ProxyError> {
        self.exhausted.store(true, Ordering::Release);
        log::warn!(
            "[Aggregation Retry] Exhausted Provider={} Model={} Reason={reason} Retries={retries} Elapsed={:.3}s",
            context.provider, context.model, started.elapsed().as_secs_f64()
        );
        result
    }
}

fn last_failure(
    response: Option<ProxyResponse>,
    error: Option<&RetainedUpstreamError>,
) -> Result<ProxyResponse, ProxyError> {
    response.map(Ok).unwrap_or_else(|| {
        Err(error
            .map(RetainedUpstreamError::to_error)
            .unwrap_or_else(|| {
                ProxyError::Timeout(
                    "Aggregation retry budget exhausted without an upstream error body".to_string(),
                )
            }))
    })
}

pub(super) fn retryable_http_error_code(status: u16) -> Option<&'static str> {
    match status {
        400 => Some("http_400"),
        429 => Some("http_429"),
        500 => Some("http_500"),
        502 => Some("http_502"),
        503 => Some("http_503"),
        504 => Some("http_504"),
        _ => None,
    }
}

async fn retain_retryable_response(response: ProxyResponse, deadline: Instant) -> ProxyResponse {
    // JSON/SSE error prefixes have already been bounded by the priming guard.
    // Preserve their original HTTP 2xx status and complete error event on exhaustion.
    if response.status().is_success() {
        return response;
    }
    let status = response.status();
    let headers = response.headers().clone();
    let mut body = BytesMut::new();
    let read = async {
        let stream = response.bytes_stream();
        tokio::pin!(stream);
        while let Some(Ok(chunk)) = stream.next().await {
            let remaining = MAX_RETRY_ERROR_BODY_BYTES - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if body.len() == MAX_RETRY_ERROR_BODY_BYTES {
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    // Slow/unfinished/oversized error bodies cannot defeat the retry deadline.
    // A body-read error still preserves the actual HTTP status and available bytes.
    let _ = tokio::time::timeout_at(deadline, read).await;
    ProxyResponse::buffered(status, headers, Bytes::from(body))
}

/// Keep transport status/headers/body for delivery while decoding a bounded copy for
/// the shared rectifier. In particular error.code=null must not discard error.message.
pub(super) async fn buffer_http_error(
    response: ProxyResponse,
    deadline: Instant,
) -> (ProxyResponse, ProxyError) {
    use super::content_encoding::{decompress_body_with_limit, get_content_encoding};
    let response = retain_retryable_response(response, deadline).await;
    let status = response.status();
    let headers = response.headers().clone();
    let raw = response
        .bytes_with_limit(MAX_RETRY_ERROR_BODY_BYTES)
        .await
        .unwrap_or_default();
    let decoded = get_content_encoding(&headers)
        .and_then(|encoding| {
            decompress_body_with_limit(&encoding, &raw, MAX_RETRY_ERROR_BODY_BYTES)
                .ok()
                .flatten()
        })
        .map(Bytes::from)
        .unwrap_or_else(|| raw.clone());
    let error = ProxyError::UpstreamError {
        status: status.as_u16(),
        body: String::from_utf8(decoded.to_vec()).ok(),
    };
    (ProxyResponse::buffered(status, headers, raw), error)
}

enum Attempt {
    Complete(ProxyResponse, bool),
    Retry(ProxyResponse, &'static str),
    HttpRetry(ProxyResponse, ProxyError, &'static str),
    OpaqueRejected(ProxyResponse, ProxyError, OpaqueStateRejection),
    PrimingTimedOut(ProxyResponse),
}

async fn run_attempt<Fut>(
    send: Fut,
    request: Arc<PreparedRequest<'_>>,
    started: &OnceLock<Instant>,
    budget: Duration,
    attempt_number: u64,
    consecutive_failures: u32,
) -> (u32, Result<Result<Attempt, ProxyError>, tokio::time::error::Elapsed>)
where
    Fut: Future<Output = Result<ProxyResponse, ProxyError>>,
{
    let attempt_started = Instant::now();
    let deadline = started.get().map(|start| *start + budget);
    let attempt = async {
        let response = send.await?;
        if !response.status().is_success() {
            started.get_or_init(Instant::now);
        }
        let probe_deadline = started.get().copied().unwrap_or_else(Instant::now) + budget;
        match inspect_attempt(response, &request, probe_deadline).await? {
            Attempt::Retry(response, code) if !response.status().is_success() => {
                // Reading a slow error body belongs to this lane so another lane
                // can make progress and win while it is being read.
                let (response, error) = buffer_http_error(response, probe_deadline).await;
                Ok(Attempt::HttpRetry(response, error, code))
            }
            outcome => Ok(outcome),
        }
    };
    let result = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, attempt).await,
        None => Ok(attempt.await),
    };
    let Some(log) = request.context.error_log else { return (consecutive_failures, result); };
    let elapsed = attempt_started.elapsed();
    let result = match result {
        Ok(Ok(Attempt::HttpRetry(response, error, code))) => {
            log.record_proxy(&error, Some(response.headers()), attempt_number, elapsed).await;
            Ok(Ok(Attempt::HttpRetry(response, error, code)))
        }
        Ok(Ok(Attempt::OpaqueRejected(response, error, rejection))) => {
            log.record_proxy(&error, Some(response.headers()), attempt_number, elapsed).await;
            Ok(Ok(Attempt::OpaqueRejected(response, error, rejection)))
        }
        Ok(Ok(Attempt::Retry(response, code))) => {
            let response = log.record_response(response, code, attempt_number, elapsed).await;
            Ok(Ok(Attempt::Retry(response, code)))
        }
        Ok(Ok(Attempt::Complete(response, false))) => {
            let code = if response.status().is_success() { "incomplete_response" } else { "upstream_http_error" };
            let response = log.record_response(response, code, attempt_number, elapsed).await;
            Ok(Ok(Attempt::Complete(response, false)))
        }
        Ok(Err(error)) => {
            log.record_proxy(&error, None, attempt_number, elapsed).await;
            Ok(Err(error))
        }
        Err(error) => {
            log.record(None, "timeout", Some("Retry attempt exceeded its time budget"), None, attempt_number, elapsed).await;
            Err(error)
        }
        outcome => outcome,
    };
    (consecutive_failures, result)
}

enum PrefixDecision {
    Pending,
    Complete(bool),
    Retry(&'static str),
    OpaqueRejected(ProxyError, OpaqueStateRejection),
}

async fn inspect_attempt(
    response: ProxyResponse,
    request: &PreparedRequest<'_>,
    deadline: Instant,
) -> Result<Attempt, ProxyError> {
    let status = response.status();
    let response = if request.context.app_type == "codex"
        && request.context.native_responses
        && request.context.rectifier.enabled
        && matches!(status.as_u16(), 400..=499 | 502)
    {
        let (response, error) = buffer_http_error(response, deadline).await;
        if let Some(rejection) = request.detect(&error) {
            return Ok(Attempt::OpaqueRejected(response, error, rejection));
        }
        response
    } else {
        response
    };
    if request.context.app_type == "claude" && status.as_u16() == 529 {
        return Ok(Attempt::Retry(response, "http_529"));
    }
    if let Some(code) = retryable_http_error_code(status.as_u16()) {
        return Ok(Attempt::Retry(response, code));
    }
    // HTTP status remains authoritative outside the explicitly requested streaming
    // error envelopes. Content from other endpoints is never searched for keywords.
    if !request.context.inspect_stream_start
        || !status.is_success()
        || super::content_encoding::get_content_encoding(response.headers()).is_some()
    {
        return Ok(Attempt::Complete(response, status.is_success()));
    }

    let headers = response.headers().clone();
    let mut stream = Box::pin(response.bytes_stream());
    let mut prefix = BytesMut::new();
    let mut sse = super::sse::SseBuffer::new();
    loop {
        if Instant::now() >= deadline {
            return Ok(Attempt::PrimingTimedOut(replay_response(
                status,
                headers,
                prefix.freeze(),
                stream,
            )));
        }
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                return Ok(Attempt::PrimingTimedOut(replay_response(
                    status,
                    headers,
                    prefix.freeze(),
                    stream,
                )));
            }
        };
        let Some(chunk) = next else {
            // A gateway may omit the blank line after its final SSE event.
            match inspect_sse_block(&sse.take_tail(), request) {
                PrefixDecision::Retry(code) => {
                    return Ok(Attempt::Retry(
                        ProxyResponse::buffered(status, headers, prefix.freeze()),
                        code,
                    ))
                }
                PrefixDecision::OpaqueRejected(error, rejection) => {
                    return Ok(Attempt::OpaqueRejected(
                        ProxyResponse::buffered(status, headers, prefix.freeze()),
                        error,
                        rejection,
                    ))
                }
                _ => {}
            }
            return Ok(Attempt::Complete(
                ProxyResponse::buffered(status, headers, prefix.freeze()),
                false,
            ));
        };
        let chunk = chunk.map_err(|error| {
            ProxyError::ForwardFailed(format!(
                "Failed while inspecting Responses stream start: {error}"
            ))
        })?;
        if chunk.len() > MAX_RESPONSE_PREFIX_BYTES - prefix.len() {
            // Commit on the memory limit. Keep this incoming chunk in the replay
            // stream without copying it into the bounded inspection buffer.
            let stream = futures::stream::once(std::future::ready(Ok(chunk))).chain(stream);
            return Ok(Attempt::Complete(
                replay_response(status, headers, prefix.freeze(), stream),
                true,
            ));
        }
        prefix.extend_from_slice(&chunk);
        sse.push_bytes(&chunk);

        // Compatible gateways sometimes return JSON even for stream:true or omit
        // the JSON content type. Only structured error objects are retry candidates.
        let decision = if matches!(
            prefix.iter().find(|byte| !byte.is_ascii_whitespace()),
            Some(b'{' | b'[')
        ) {
            match serde_json::from_slice::<Value>(&prefix) {
                Ok(value) => inspect_json_value(&value, request),
                Err(_) => PrefixDecision::Pending,
            }
        } else {
            let mut decision = PrefixDecision::Pending;
            while let Some(block) = sse.next_block() {
                decision = inspect_sse_block(block, request);
                if !matches!(decision, PrefixDecision::Pending) {
                    break;
                }
            }
            decision
        };

        match decision {
            PrefixDecision::OpaqueRejected(error, rejection) => {
                return Ok(Attempt::OpaqueRejected(
                    ProxyResponse::buffered(status, headers, prefix.freeze()),
                    error,
                    rejection,
                ));
            }
            PrefixDecision::Retry(code) => {
                return Ok(Attempt::Retry(
                    ProxyResponse::buffered(status, headers, prefix.freeze()),
                    code,
                ));
            }
            PrefixDecision::Complete(successful) => {
                // The first output item, text delta or tool event commits the response.
                // Later stream errors are passed through and cannot re-enter send().
                return Ok(Attempt::Complete(
                    replay_response(status, headers, prefix.freeze(), stream),
                    successful,
                ));
            }
            PrefixDecision::Pending => tokio::task::yield_now().await,
        }
    }
}

fn replay_response<S>(
    status: http::StatusCode,
    headers: http::HeaderMap,
    prefix: Bytes,
    stream: S,
) -> ProxyResponse
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let replay = futures::stream::once(std::future::ready(Ok(prefix))).chain(stream);
    ProxyResponse::streamed(status, headers, replay)
}

fn inspect_json_value(value: &Value, request: &PreparedRequest<'_>) -> PrefixDecision {
    if has_output(value) {
        return PrefixDecision::Complete(response_error(value).is_none());
    }
    match response_error(value) {
        Some(error) => inspect_error(error, request),
        None => PrefixDecision::Complete(true),
    }
}

fn inspect_sse_block(block: &str, request: &PreparedRequest<'_>) -> PrefixDecision {
    let mut named_event = None;
    let mut data = Vec::new();
    for line in block.lines() {
        if let Some(event) = super::sse::strip_sse_field(line, "event") {
            named_event = Some(event.trim());
        } else if let Some(line) = super::sse::strip_sse_field(line, "data") {
            data.push(line);
        }
    }
    if data.is_empty() {
        return PrefixDecision::Pending;
    }
    let Ok(value) = serde_json::from_str::<Value>(&data.join("\n")) else {
        // Unknown data (including [DONE]) must be exposed as-is. Retrying after
        // unreadable output could repeat content or a tool call.
        return PrefixDecision::Complete(true);
    };
    let event = named_event
        .filter(|event| !event.is_empty())
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("");
    if request.context.app_type == "claude" {
        // Anthropic lifecycle events are not productive output. Preserve early
        // rate-limit/overload retries until text/thinking/tool blocks begin.
        if matches!(event, "message_start" | "ping") {
            let message = value.get("message").unwrap_or(&value);
            if message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|content| !content.is_empty())
            {
                return PrefixDecision::Complete(response_error(&value).is_none());
            }
            if let Some(error) = response_error(&value) {
                return inspect_error(error, request);
            }
            return PrefixDecision::Pending;
        }
        // Compatible Chat Completions providers can send a role-only chunk before
        // an error. Do not commit that metadata as user-visible output.
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            let metadata_only = !choices.is_empty()
                && choices.iter().all(|choice| {
                    choice.get("finish_reason").is_none_or(Value::is_null)
                        && choice
                            .get("delta")
                            .and_then(Value::as_object)
                            .is_some_and(|delta| {
                                delta.iter().all(|(field, value)| {
                                    field == "role"
                                        || value.is_null()
                                        || value.as_str().is_some_and(str::is_empty)
                                })
                            })
                });
            if metadata_only {
                return match response_error(&value) {
                    Some(error) => inspect_error(error, request),
                    None => PrefixDecision::Pending,
                };
            }
        }
    }
    if has_output(&value)
        || !matches!(
            event,
            "" | "response.created"
                | "response.in_progress"
                | "response.queued"
                | "response.failed"
                | "error"
        )
    {
        return PrefixDecision::Complete(response_error(&value).is_none());
    }
    let error = response_error(&value).or_else(|| {
        matches!(event, "response.failed" | "error")
            .then_some(value.get("response").unwrap_or(&value))
    });
    if let Some(error) = error {
        return inspect_error(error, request);
    }
    match event {
        "response.created" | "response.in_progress" | "response.queued" => PrefixDecision::Pending,
        "response.failed" | "error" => PrefixDecision::Complete(false),
        _ => PrefixDecision::Complete(true),
    }
}

fn inspect_error(error: &Value, request: &PreparedRequest<'_>) -> PrefixDecision {
    let code = retryable_error_code(error);
    if let Some((error, rejection)) = request.stream_rejection(error) {
        return PrefixDecision::OpaqueRejected(error, rejection);
    }
    match code {
        Some(code) => PrefixDecision::Retry(code),
        None => PrefixDecision::Complete(false),
    }
}

fn has_output(value: &Value) -> bool {
    value
        .get("response")
        .unwrap_or(value)
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| !output.is_empty())
}

fn response_error(value: &Value) -> Option<&Value> {
    let response = value.get("response").unwrap_or(value);
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        Some(error)
    } else if matches!(
        value.get("type").and_then(Value::as_str),
        Some("error" | "response.failed")
    ) || matches!(
        response.get("status").and_then(Value::as_str),
        Some("failed" | "cancelled")
    ) {
        Some(response)
    } else {
        None
    }
}

pub(super) fn retryable_error_code(error: &Value) -> Option<&'static str> {
    for field in [
        "status",
        "status_code",
        "http_status",
        "upstream_status",
        "code",
        "type",
    ] {
        match error.get(field).and_then(Value::as_str) {
            Some("rate_limit_exceeded") => return Some("rate_limit_exceeded"),
            Some("rate_limit_error") => return Some("rate_limit_error"),
            Some("token_rate_limit_exceeded") => return Some("token_rate_limit_exceeded"),
            Some("server_error" | "internal_server_error" | "internal_error" | "api_error") => {
                return Some("upstream_server_error")
            }
            Some("overloaded" | "overloaded_error" | "model_overloaded") => {
                return Some("upstream_overloaded")
            }
            Some("service_unavailable" | "service_unavailable_error") => {
                return Some("upstream_service_unavailable")
            }
            Some("get_channel_failed") => return Some("upstream_channel_unavailable"),
            Some("429" | "HTTP 429") => return Some("wrapped_http_429"),
            Some("400" | "HTTP 400") => return Some("wrapped_http_400"),
            Some("500" | "HTTP 500") => return Some("wrapped_http_500"),
            Some("502" | "HTTP 502") => return Some("wrapped_http_502"),
            Some("503" | "HTTP 503") => return Some("wrapped_http_503"),
            Some("504" | "HTTP 504") => return Some("wrapped_http_504"),
            Some("529" | "HTTP 529") => return Some("wrapped_http_529"),
            _ => {}
        }
        match error.get(field).and_then(Value::as_u64) {
            Some(429) => return Some("wrapped_http_429"),
            Some(400) => return Some("wrapped_http_400"),
            Some(500) => return Some("wrapped_http_500"),
            Some(502) => return Some("wrapped_http_502"),
            Some(503) => return Some("wrapped_http_503"),
            Some(504) => return Some("wrapped_http_504"),
            Some(529) => return Some("wrapped_http_529"),
            _ => {}
        }
    }
    // Match specific messages inside a structured error only. Never search normal
    // generated text for loose occurrences of "rate limit", "busy" or "demand".
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())?;
    if message.contains("Your requests to ") && message.contains("have exceeded token rate limit") {
        return Some("azure_token_rate_limit");
    }
    let message_lower = message.to_ascii_lowercase();
    if message_lower.contains("currently experiencing high demand")
        && message_lower.contains("temporary errors")
    {
        return Some("upstream_high_demand");
    }
    if message.contains("当前模型 ")
        && message.contains("负载已经达到上限")
        && message.contains("请稍后重试")
    {
        return Some("upstream_model_capacity");
    }
    None
}
