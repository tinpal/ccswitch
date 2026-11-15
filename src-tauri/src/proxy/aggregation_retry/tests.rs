use super::*;
use http::{HeaderMap, StatusCode};
use serde_json::json;
use std::{collections::VecDeque, future::ready, sync::Arc, task::Poll};

const RESOURCE_MESSAGE: &str = "The requested item was created under a different Azure OpenAI resource. Use the same resource that created the item to access it.";

fn config() -> AggregationRetryConfig {
    AggregationRetryConfig {
        enabled: true,
        parallel_enabled: false,
        parallel_requests: 2,
        retry_interval_seconds: 0,
        max_retries: Some(4),
        max_elapsed_seconds: 600,
        retry_delay: Default::default(),
    }
}

#[tokio::test]
async fn upstream_errors_record_every_failure_before_retry_success() {
    use crate::database::{Database, UpstreamErrorSource};
    use crate::proxy::upstream_errors::UpstreamErrorContext;
    let db = Arc::new(Database::memory().unwrap());
    let provider = crate::provider::Provider::with_id("provider-a".into(), "Gateway".into(),
        json!({"auth":{"OPENAI_API_KEY":"private-test-key"}}), None);
    let retry = AggregationRetry::new(config());
    let log = UpstreamErrorContext::new(db.clone(), UpstreamErrorSource::AggregationRetry,
        "codex", &provider, "test-model", retry.request_id().into());
    let body = history();
    let rectifier = RectifierConfig::default();
    let mut ctx = context(&body, &rectifier);
    ctx.error_log = Some(&log);
    let mut responses = VecDeque::from([
        json_response(500, json!({"error":{"code":"get_channel_failed","message":"private-test-key unavailable"},"request_id":"upstream-1"})),
        temporary_failure(503), successful_response(),
    ]);
    let response = retry.send(ctx, Bytes::from(body.to_string()), |_| ready(Ok(responses.pop_front().unwrap()))).await.unwrap();
    assert!(response.status().is_success());
    let errors = db.get_upstream_error_logs(&Default::default(), 1, 50).unwrap();
    assert_eq!(errors.total, 2);
    assert_eq!(errors.data.iter().map(|row| row.attempt).collect::<Vec<_>>(), vec![2, 1]);
    assert!(errors.data.iter().all(|row| row.request_id == retry.request_id()));
    let detail = db.get_upstream_error_detail(errors.data[1].id).unwrap().unwrap();
    assert_eq!(detail.error_code, "get_channel_failed");
    assert_eq!(detail.upstream_request_id.as_deref(), Some("upstream-1"));
    assert!(!detail.response_body.unwrap().contains("private-test-key"));
    let count: i64 = db.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn upstream_errors_record_sse_failure_under_http_200() {
    use crate::database::{Database, UpstreamErrorSource};
    use crate::proxy::upstream_errors::UpstreamErrorContext;
    let db = Arc::new(Database::memory().unwrap());
    let provider = crate::provider::Provider::with_id("provider-a".into(), "Gateway".into(), json!({}), None);
    let retry = AggregationRetry::new(config());
    let log = UpstreamErrorContext::new(db.clone(), UpstreamErrorSource::AggregationRetry,
        "codex", &provider, "test-model", retry.request_id().into());
    let body = history();
    let rectifier = RectifierConfig::default();
    let mut ctx = context(&body, &rectifier);
    ctx.error_log = Some(&log);
    let mut responses = VecDeque::from([
        sse_response(vec!["event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"temporary failure\"}}}\n\n".into()]),
        successful_response(),
    ]);
    retry.send(ctx, Bytes::from(body.to_string()), |_| ready(Ok(responses.pop_front().unwrap()))).await.unwrap();
    let errors = db.get_upstream_error_logs(&Default::default(), 1, 50).unwrap();
    assert_eq!(errors.total, 1);
    assert_eq!(errors.data[0].status_code, Some(200));
    assert_eq!(errors.data[0].error_code, "server_error");
}

fn history() -> Value {
    json!({
        "model": "test-model", "stream": true,
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"keep user text"}]},
            {"type":"reasoning","id":"rs_foreign","encrypted_content":"test-cipher","summary":[]},
            {"type":"message","id":"msg_foreign","role":"assistant","content":[{"type":"output_text","text":"keep answer"}]},
            {"type":"function_call","id":"fc_foreign","call_id":"call_keep","name":"shell","arguments":"{\"cmd\":\"echo keep\"}"},
            {"type":"function_call_output","id":"fco_foreign","call_id":"call_keep","output":[
                {"type":"input_text","text":"keep tool result"},
                {"type":"encrypted_content","encrypted_content":"test-encrypted-result"}
            ]},
            {"type":"item_reference","id":"msg_reference"},
            {"role":"user","content":[{"type":"input_text","text":"continue"}]}
        ]
    })
}

fn context<'a>(body: &'a Value, rectifier: &'a RectifierConfig) -> AggregationRequest<'a> {
    AggregationRequest {
        app_type: "codex",
        provider: "provider-a",
        model: "test-model",
        native_responses: true,
        inspect_stream_start: true,
        body,
        rectifier,
        codex_third_party: true,
        opaque_scope: None,
        error_log: None,
    }
}

fn json_response(status: u16, body: Value) -> ProxyResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    ProxyResponse::buffered(
        StatusCode::from_u16(status).unwrap(),
        headers,
        Bytes::from(body.to_string()),
    )
}

fn resource_error() -> Value {
    json!({"error":{"message":RESOURCE_MESSAGE,"type":"invalid_request_error","code":null}})
}

fn resource_failure() -> ProxyResponse {
    json_response(400, resource_error())
}

fn successful_response() -> ProxyResponse {
    json_response(
        200,
        json!({"id":"resp_success","object":"response","status":"completed","output":[]}),
    )
}

fn temporary_failure(status: u16) -> ProxyResponse {
    json_response(
        status,
        json!({"error":{"type":"server_error","message":"temporarily unavailable"}}),
    )
}

fn sse_response(parts: Vec<String>) -> ProxyResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "text/event-stream".parse().unwrap(),
    );
    ProxyResponse::streamed(
        StatusCode::OK,
        headers,
        futures::stream::iter(parts.into_iter().map(|s| Ok(Bytes::from(s)))),
    )
}

fn failed_event() -> String {
    format!(
        "event: response.failed\ndata: {}\n\n",
        json!({
            "type":"response.failed", "response":{"status":"failed","output":[],"error":resource_error()["error"]}
        })
    )
}

fn completed_event() -> String {
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_success\",\"status\":\"completed\",\"output\":[]}}\n\n".to_string()
}

fn rate_limit_event() -> String {
    format!(
        "event: response.failed\ndata: {}\n\n",
        json!({
            "type":"response.failed",
            "response":{"status":"failed","output":[],"error":{
                "code":"rate_limit_exceeded","message":"rate limit"
            }}
        })
    )
}

async fn run(
    retry: &AggregationRetry,
    context: AggregationRequest<'_>,
    steps: Vec<ProxyResponse>,
) -> (Result<ProxyResponse, ProxyError>, Vec<Bytes>) {
    let mut script = VecDeque::from(steps);
    let mut sent = Vec::new();
    let bytes = Bytes::from(context.body.to_string());
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        retry.send(context, bytes, |body| {
            sent.push(body);
            ready(Ok(script.pop_front().expect("unexpected upstream retry")))
        }),
    )
    .await
    .expect("retry must not wait for the 600-second budget");
    (result, sent)
}

fn assert_rectified(bytes: &Bytes) {
    let body: Value = serde_json::from_slice(bytes).unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 6);
    assert!(input.iter().all(|item| item["type"] != "reasoning"));
    assert!(input
        .iter()
        .filter(|item| item["type"] != "item_reference")
        .all(|item| item.get("id").is_none()));
    assert_eq!(input[0]["content"][0]["text"], "keep user text");
    assert_eq!(input[1]["content"][0]["text"], "keep answer");
    assert_eq!(input[2]["arguments"], "{\"cmd\":\"echo keep\"}");
    assert_eq!(input[2]["call_id"], "call_keep");
    assert_eq!(input[3]["call_id"], "call_keep");
    assert_eq!(input[3]["output"][0]["text"], "keep tool result");
    assert_eq!(input[3]["output"][1]["text"], "[encrypted content omitted]");
    assert_eq!(input[4]["id"], "msg_reference");
}

#[tokio::test]
async fn null_code_resource_error_rectifies_before_ordinary_retry() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(config());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![resource_failure(), successful_response()],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], Bytes::from(request.to_string()));
    assert_rectified(&sent[1]);
    assert_eq!(retry.state.lock().await.retries, 1);
}

#[tokio::test]
async fn ordinary_errors_after_rectification_keep_the_repaired_body() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(config());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![
            resource_failure(),
            temporary_failure(503),
            temporary_failure(429),
            successful_response(),
        ],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent.len(), 4);
    assert_rectified(&sent[1]);
    assert_eq!(sent[1], sent[2]);
    assert_eq!(sent[2], sent[3]);
}

#[tokio::test]
async fn repeated_resource_errors_rectify_idempotently_until_success() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        max_retries: None,
        ..config()
    });
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![
            resource_failure(),
            resource_failure(),
            temporary_failure(500),
            resource_failure(),
            successful_response(),
        ],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent.len(), 5);
    assert_rectified(&sent[1]);
    assert!(sent[1..].iter().all(|body| body == &sent[1]));
    assert_eq!(retry.state.lock().await.retries, 4);
    assert!(!retry.was_exhausted());
}

#[tokio::test]
async fn no_removable_state_still_retries_and_preserves_item_references() {
    let request = json!({"input":[{"type":"item_reference","id":"msg_only_reference"}]});
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(config());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![
            resource_failure(),
            resource_failure(),
            successful_response(),
        ],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent.len(), 3);
    assert!(sent
        .iter()
        .all(|body| body == &Bytes::from(request.to_string())));
}

#[tokio::test]
async fn disabled_rectifier_and_non_native_protocol_keep_ordinary_policy() {
    for native in [false, true] {
        let request = history();
        let rectifier = RectifierConfig {
            enabled: !native,
            ..Default::default()
        };
        let retry = AggregationRetry::new(config());
        let mut ctx = context(&request, &rectifier);
        ctx.native_responses = native;
        ctx.inspect_stream_start = native;
        let (result, sent) =
            run(&retry, ctx, vec![resource_failure(), successful_response()]).await;
        assert!(result.is_ok());
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], sent[1]);
    }
}

#[tokio::test]
async fn disabled_aggregation_returns_the_first_error_unchanged() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        enabled: false,
        ..config()
    });
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![resource_failure()],
    )
    .await;
    let response = result.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        response.bytes_with_limit(4096).await.unwrap(),
        Bytes::from(resource_error().to_string())
    );
}

#[tokio::test]
async fn ordinary_status_retry_and_terminal_status_policy_are_unchanged() {
    for status in [400, 429, 500, 502, 503, 504, 401, 403, 422] {
        let request = history();
        let rectifier = RectifierConfig::default();
        let retry = AggregationRetry::new(config());
        let (result, sent) = run(
            &retry,
            context(&request, &rectifier),
            vec![temporary_failure(status), successful_response()],
        )
        .await;
        let expected = if retryable_http_error_code(status).is_some() {
            2
        } else {
            1
        };
        assert_eq!(sent.len(), expected, "status={status}");
        assert!(result.is_ok());
        assert!(sent.iter().all(|body| body == &sent[0]));
    }
}

#[tokio::test]
async fn provider_state_survives_compatibility_reentry_and_is_request_scoped() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        max_retries: None,
        ..config()
    });
    for provider in ["provider-a", "provider-b"] {
        let mut ctx = context(&request, &rectifier);
        ctx.provider = provider;
        let (result, sent) =
            run(&retry, ctx, vec![resource_failure(), successful_response()]).await;
        assert!(result.is_ok());
        assert_eq!(sent[0], Bytes::from(request.to_string()));
        assert_rectified(&sent[1]);
    }
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![temporary_failure(503), successful_response()],
    )
    .await;
    assert!(result.is_ok());
    assert_rectified(&sent[0]);
    assert_eq!(sent[0], sent[1]);
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![resource_failure(), successful_response()],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent.len(), 2);
    assert_rectified(&sent[0]);
    assert_eq!(sent[0], sent[1]);
    let fresh_request = AggregationRetry::new(config());
    let (result, sent) = run(
        &fresh_request,
        context(&request, &rectifier),
        vec![resource_failure(), successful_response()],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent[0], Bytes::from(request.to_string()));
}

#[tokio::test]
async fn native_sse_and_json_failures_before_output_use_the_same_rectifier() {
    for framing in ["failed", "error", "json", "unterminated", "split"] {
        let failure = match framing {
            "error" => sse_response(vec![format!(
                "event: error\ndata: {}\n\n",
                resource_error()["error"]
            )]),
            "json" => json_response(
                200,
                json!({"status":"failed","output":[],"error":resource_error()["error"]}),
            ),
            "unterminated" => sse_response(vec![failed_event().trim_end().to_string()]),
            "split" => {
                let event = failed_event();
                sse_response(vec![event[..35].to_string(), event[35..].to_string()])
            }
            _ => sse_response(vec![
                "event: response.created\ndata: {\"type\":\"response.created\"}\n\n".into(),
                failed_event(),
            ]),
        };
        let request = history();
        let rectifier = RectifierConfig::default();
        let retry = AggregationRetry::new(AggregationRetryConfig { ..config() });
        let (result, sent) = run(
            &retry,
            context(&request, &rectifier),
            vec![failure, sse_response(vec![completed_event()])],
        )
        .await;
        assert!(result.is_ok(), "{framing}");
        assert_eq!(sent.len(), 2);
        assert_rectified(&sent[1]);
    }
}

#[tokio::test]
async fn ordinary_sse_never_retries_after_text_or_tool_output() {
    for event in [
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"keep text\"}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_once\",\"arguments\":\"{}\"}}\n\n",
    ] {
        for tail in [
            failed_event(),
            rate_limit_event(),
            "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"output\":[]}}\n\n".into(),
            String::new(),
        ] {
            for one_chunk in [false, true] {
                let body = format!("{event}{tail}");
                let parts = if one_chunk {
                    vec![body.clone()]
                } else {
                    vec![event.to_string(), tail.clone()]
                };
                let request = history();
                let rectifier = RectifierConfig::default();
                let retry = AggregationRetry::new(config());
                let (result, sent) = run(
                    &retry,
                    context(&request, &rectifier),
                    vec![sse_response(parts)],
                ).await;
                assert_eq!(sent.len(), 1);
                assert_eq!(
                    result.unwrap().bytes_with_limit(8192).await.unwrap(),
                    Bytes::from(body)
                );
            }
        }
    }
}

#[tokio::test]
async fn stream_start_rate_limit_retries_keep_the_rectified_body() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(config());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![
            resource_failure(),
            sse_response(vec![
                "event: response.created\ndata: {\"type\":\"response.created\"}\n\n".into(),
                rate_limit_event(),
            ]),
            sse_response(vec![completed_event()]),
        ],
    )
    .await;
    assert_eq!(sent.len(), 3);
    assert_rectified(&sent[1]);
    assert_eq!(sent[1], sent[2]);
    assert_eq!(retry.state.lock().await.retries, 2);
    assert_eq!(
        result.unwrap().bytes_with_limit(8192).await.unwrap(),
        Bytes::from(completed_event())
    );
}

#[tokio::test(start_paused = true)]
async fn legacy_success_only_setting_delivers_output_before_stream_completion() {
    let config: AggregationRetryConfig = serde_json::from_value(json!({
        "enabled":true, "successOnly":true, "maxRetries":7, "maxElapsedSeconds":45
    }))
    .unwrap();
    config.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        json!({"enabled":true,"parallelEnabled":false,"parallelRequests":2,"retryIntervalSeconds":0,"maxRetries":7,"maxElapsedSeconds":45})
    );
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(config);
    let output = Bytes::from_static(
        b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"visible now\"}\n\n"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "text/event-stream".parse().unwrap(),
    );
    let response = ProxyResponse::streamed(
        StatusCode::OK,
        headers,
        futures::stream::once(ready(Ok(output.clone()))).chain(futures::stream::pending()),
    );
    let start = Instant::now();
    let (result, sent) = run(&retry, context(&request, &rectifier), vec![response]).await;
    let mut stream = Box::pin(result.unwrap().bytes_stream());
    assert_eq!(stream.next().await.unwrap().unwrap(), output);
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(sent.len(), 1);
    drop(stream);
}

#[tokio::test]
async fn transport_error_after_output_is_delivered_without_retry() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(config());
    let output = Bytes::from_static(
        b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"keep this\"}\n\n"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "text/event-stream".parse().unwrap(),
    );
    let response = ProxyResponse::streamed(
        StatusCode::OK,
        headers,
        futures::stream::iter(vec![
            Ok(output.clone()),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "upstream disconnected",
            )),
        ]),
    );
    let (result, sent) = run(&retry, context(&request, &rectifier), vec![response]).await;
    let mut stream = Box::pin(result.unwrap().bytes_stream());
    assert_eq!(stream.next().await.unwrap().unwrap(), output);
    assert_eq!(
        stream.next().await.unwrap().unwrap_err().kind(),
        std::io::ErrorKind::ConnectionReset
    );
    assert!(stream.next().await.is_none());
    assert_eq!(sent.len(), 1);
}

#[tokio::test]
async fn rectification_spends_the_existing_retry_count_budget() {
    for limit in [0, 1, 2] {
        let request = history();
        let rectifier = RectifierConfig::default();
        let retry = AggregationRetry::new(AggregationRetryConfig {
            max_retries: Some(limit),
            ..config()
        });
        let (result, sent) = run(
            &retry,
            context(&request, &rectifier),
            vec![
                resource_failure(),
                temporary_failure(503),
                successful_response(),
            ],
        )
        .await;
        assert_eq!(sent.len(), limit as usize + 1);
        let expected_status = [400, 503, 200][limit as usize];
        assert_eq!(result.unwrap().status().as_u16(), expected_status);
        assert_eq!(retry.was_exhausted(), limit < 2);
        if limit > 0 {
            assert_rectified(&sent[1]);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn elapsed_budget_covers_rectification_and_later_hung_attempts() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        max_retries: None,
        max_elapsed_seconds: 1,
        ..config()
    });
    let start = Instant::now();
    let mut sent = Vec::new();
    let response = retry
        .send(
            context(&request, &rectifier),
            Bytes::from(request.to_string()),
            |body| {
                sent.push(body);
                let attempt = sent.len();
                async move {
                    match attempt {
                        1 => Ok(resource_failure()),
                        2 => {
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            Ok(temporary_failure(503))
                        }
                        _ => std::future::pending().await,
                    }
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(start.elapsed(), Duration::from_secs(1));
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1], sent[2]);
    assert!(retry.was_exhausted());
}

#[tokio::test(start_paused = true)]
async fn unfinished_http_error_body_cannot_overrun_the_budget() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        max_elapsed_seconds: 1,
        ..config()
    });
    let mut calls = 0;
    let start = Instant::now();
    let response = retry
        .send(
            context(&request, &rectifier),
            Bytes::from(request.to_string()),
            |_| {
                calls += 1;
                ready(Ok(ProxyResponse::streamed(
                    StatusCode::BAD_REQUEST,
                    HeaderMap::new(),
                    futures::stream::once(ready(Ok(Bytes::from(resource_error().to_string()))))
                        .chain(futures::stream::pending()),
                )))
            },
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(calls, 1);
    assert_eq!(start.elapsed(), Duration::from_secs(1));
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_drops_the_rectified_send() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig { ..config() });
    let dropped = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut future = Box::pin(retry.send(
        context(&request, &rectifier),
        Bytes::from(request.to_string()),
        |body| {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            let dropped = dropped.clone();
            let entered = entered.clone();
            async move {
                if attempt < 2 {
                    return Ok(resource_failure());
                }
                assert_rectified(&body);
                let _guard = DropFlag(dropped);
                entered.store(true, Ordering::SeqCst);
                std::future::pending().await
            }
        },
    ));
    for _ in 0..64 {
        assert!(matches!(futures::poll!(future.as_mut()), Poll::Pending));
        if entered.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(entered.load(Ordering::SeqCst));
    drop(future);
    assert!(dropped.load(Ordering::SeqCst));
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn compaction_fallback_and_encrypted_agent_protection_are_reused() {
    let rectifier = RectifierConfig::default();
    let mut request = history();
    request["input"].as_array_mut().unwrap().insert(
        0,
        json!({"type":"compaction","encrypted_content":"test-official-summary"}),
    );
    let retry = AggregationRetry::new(config());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![
            json_response(422, json!({"error":{"message":"unsupported item"}})),
            successful_response(),
        ],
    )
    .await;
    assert!(result.is_ok());
    let repaired: Value = serde_json::from_slice(&sent[1]).unwrap();
    assert_ne!(repaired["input"][0]["type"], "compaction");
    assert!(repaired["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "reasoning"));

    let task = json!({"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"test-protected-task"}]});
    request = history();
    request["input"].as_array_mut().unwrap().push(task.clone());
    let retry = AggregationRetry::new(config());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![
            resource_failure(),
            resource_failure(),
            successful_response(),
        ],
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(sent[1], sent[2]);
    let repaired: Value = serde_json::from_slice(&sent[1]).unwrap();
    assert_eq!(repaired["input"].as_array().unwrap().last(), Some(&task));
}

#[tokio::test]
async fn compressed_http_error_preserves_message_detection() {
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(resource_error().to_string().as_bytes())
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_ENCODING, "gzip".parse().unwrap());
    let failure = ProxyResponse::buffered(
        StatusCode::BAD_REQUEST,
        headers,
        Bytes::from(gzip.finish().unwrap()),
    );
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(config());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![failure, successful_response()],
    )
    .await;
    assert!(result.is_ok());
    assert_rectified(&sent[1]);
}

#[tokio::test(start_paused = true)]
async fn persistent_resource_errors_exhaust_one_600_second_budget() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        max_retries: None,
        max_elapsed_seconds: 600,
        ..config()
    });
    let mut sent = Vec::new();
    let result = retry
        .send(
            context(&request, &rectifier),
            Bytes::from(request.to_string()),
            |body| {
                sent.push(body);
                let attempt = sent.len();
                async move {
                    tokio::time::sleep(Duration::from_secs(250)).await;
                    let mut error = resource_error();
                    error["error"]["attempt"] = json!(attempt);
                    Ok(json_response(400, error))
                }
            },
        )
        .await;
    let body = match result {
        Ok(response) => {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            response.bytes_with_limit(4096).await.unwrap()
        }
        Err(ProxyError::UpstreamError {
            status: 400,
            body: Some(body),
        }) => Bytes::from(body),
        _ => panic!("expected the last real upstream resource error"),
    };
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["message"], RESOURCE_MESSAGE);
    assert!(error["error"]["code"].is_null());
    assert_eq!(error["error"]["attempt"], 3);
    assert_eq!(sent.len(), 4);
    assert!(sent[1..].iter().all(|body| body == &sent[1]));
    assert_rectified(&sent[1]);
    let state = retry.state.lock().await;
    assert_eq!(state.retries, 3);
    assert_eq!(
        state.started.get().unwrap().elapsed(),
        Duration::from_secs(600)
    );
    assert!(retry.was_exhausted());
}

#[tokio::test]
async fn repeated_rectification_honors_the_shared_count_limit() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        max_retries: Some(2),
        ..config()
    });
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![resource_failure(), resource_failure(), resource_failure()],
    )
    .await;
    assert_eq!(sent.len(), 3);
    let response = result.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.bytes_with_limit(4096).await.unwrap(),
        Bytes::from(resource_error().to_string())
    );
    assert!(retry.was_exhausted());
    assert_eq!(retry.state.lock().await.retries, 2);
    assert_eq!(sent[1], sent[2]);
}

#[tokio::test]
async fn exhaustion_keeps_the_last_meaningful_error_after_an_empty_failure() {
    let request = history();
    let rectifier = RectifierConfig::default();
    let retry = AggregationRetry::new(AggregationRetryConfig {
        max_retries: Some(1),
        ..config()
    });
    let empty = ProxyResponse::buffered(StatusCode::BAD_REQUEST, HeaderMap::new(), Bytes::new());
    let (result, sent) = run(
        &retry,
        context(&request, &rectifier),
        vec![resource_failure(), empty],
    )
    .await;
    assert_eq!(sent.len(), 2);
    let body = match result {
        Ok(response) => response.bytes_with_limit(4096).await.unwrap(),
        Err(ProxyError::UpstreamError {
            status: 400,
            body: Some(body),
        }) => Bytes::from(body),
        _ => panic!("last meaningful error must survive"),
    };
    assert_eq!(body, Bytes::from(resource_error().to_string()));
}
