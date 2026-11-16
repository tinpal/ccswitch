//! Independent, opt-in model probes. The proxy owns the scheduler and every worker.
//! Provider model rows are the source of truth; no conversation is replayed here.

mod client_templates;
mod probe;
mod prompt;
mod response;

pub(super) use prompt::minimize_prompt;

/// In-process provenance. A network client cannot opt into background template
/// handling by sending a header.
#[derive(Clone, Copy, Debug)]
pub(super) struct NativeClientRequest {
    pub use_minimal_prompt: bool,
}

use super::server::ProxyState;
use crate::{
    app_config::AppType,
    database::Database,
    live::engine::DeviceStore,
    mode::state,
    provider::{ModelKeepAliveConfig, Provider},
};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{watch, Semaphore},
    task::{AbortHandle, JoinSet},
    time::{sleep_until, Instant, MissedTickBehavior},
};

pub const MAX_BACKGROUND_REQUESTS: usize = 16;
type TaskKey = (String, String, String);
type ProviderKey = (String, String);
type SharedStatus = Arc<Mutex<ModelKeepAliveStatus>>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelKeepAliveStatus {
    pub app_type: String,
    pub provider_id: String,
    pub model: String,
    pub config: ModelKeepAliveConfig,
    /// acquiring / keepalive
    pub mode: String,
    /// disabled / paused / queued / requesting / waiting / retry_wait / error
    pub phase: String,
    pub reason: Option<String>,
    pub active_requests: u32,
    pub total_requests: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub last_http_status: Option<u16>,
    pub next_request_at: Option<String>,
    pub client_user_agent: Option<String>,
}

#[derive(Clone)]
struct TaskSpec {
    app: AppType,
    provider: Provider,
    model: String,
    config: ModelKeepAliveConfig,
    pause_reason: Option<&'static str>,
}

impl TaskSpec {
    fn key(&self) -> TaskKey {
        (
            self.app.as_str().into(),
            self.provider.id.clone(),
            self.model.clone(),
        )
    }

    fn fingerprint(&self) -> [u8; 32] {
        // Credentials affect the task generation, but neither the digest nor its
        // input is exposed to the UI/log. Do not retain a serialized secret copy.
        let mut settings = self.provider.settings_config.clone();
        if let Some(models) = settings
            .pointer_mut("/modelCatalog/models")
            .and_then(serde_json::Value::as_array_mut)
        {
            for model in models {
                if let Some(object) = model.as_object_mut() {
                    object.remove("keepAlive");
                }
            }
        }
        let mut meta = self.provider.meta.clone();
        if let Some(models) = meta.as_mut().and_then(|meta| meta.stack_models.as_mut()) {
            for model in models {
                model.keep_alive = None;
            }
        }
        // Editing another model's timer or this provider's label must not restart
        // otherwise healthy keep-alive workers.
        let value = serde_json::json!([
            self.provider.category,
            settings,
            meta,
            self.model,
            self.config
        ]);
        // ProviderMeta contains HashMaps whose iteration order changes on each
        // DB read. Canonicalize nested objects before hashing to avoid restarting
        // the worker every reconciliation tick for an unchanged provider.
        Sha256::digest(super::json_canonical::canonical_json_string(&value).as_bytes()).into()
    }

    fn status(&self) -> ModelKeepAliveStatus {
        ModelKeepAliveStatus {
            app_type: self.app.as_str().into(),
            provider_id: self.provider.id.clone(),
            model: self.model.clone(),
            config: self.config.clone(),
            mode: "acquiring".into(),
            phase: if !self.config.enabled {
                "disabled"
            } else {
                "paused"
            }
            .into(),
            reason: self.pause_reason.map(str::to_string),
            active_requests: 0,
            total_requests: 0,
            success_count: 0,
            failure_count: 0,
            last_attempt_at: None,
            last_success_at: None,
            last_error: None,
            last_http_status: None,
            next_request_at: None,
            client_user_agent: None,
        }
    }
}

/// Shared status and admission control. Foreground requests never wait for a
/// background permit; their arrival cancels probes on that same provider.
pub struct KeepAliveRuntime {
    statuses: Mutex<BTreeMap<TaskKey, SharedStatus>>,
    foreground: watch::Sender<HashMap<ProviderKey, usize>>,
    slots: Arc<Semaphore>,
    client_templates: client_templates::ClientTemplates,
}

impl Default for KeepAliveRuntime {
    fn default() -> Self {
        let (foreground, _) = watch::channel(HashMap::new());
        Self {
            statuses: Mutex::new(BTreeMap::new()),
            foreground,
            slots: Arc::new(Semaphore::new(MAX_BACKGROUND_REQUESTS)),
            client_templates: client_templates::ClientTemplates::default(),
        }
    }
}

impl KeepAliveRuntime {
    pub fn snapshot(&self, app: &str, provider_id: &str) -> Vec<ModelKeepAliveStatus> {
        self.statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|status| status.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .filter(|status| status.app_type == app && status.provider_id == provider_id)
            .collect()
    }

    pub(super) fn enter_foreground(
        self: &Arc<Self>,
        app: &str,
        providers: &[Provider],
    ) -> ForegroundGuard {
        let keys: HashSet<_> = providers
            .iter()
            .map(|provider| (app.to_string(), provider.id.clone()))
            .collect();
        self.foreground.send_modify(|counts| {
            for key in &keys {
                *counts.entry(key.clone()).or_default() += 1;
            }
        });
        ForegroundGuard {
            runtime: self.clone(),
            keys,
        }
    }
}

pub(super) struct ForegroundGuard {
    runtime: Arc<KeepAliveRuntime>,
    keys: HashSet<ProviderKey>,
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        self.runtime.foreground.send_modify(|counts| {
            for key in &self.keys {
                if let Some(count) = counts.get_mut(key) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        counts.remove(key);
                    }
                }
            }
        });
    }
}

fn update(status: &SharedStatus, change: impl FnOnce(&mut ModelKeepAliveStatus)) {
    change(&mut status.lock().unwrap_or_else(|e| e.into_inner()));
}

fn stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn due_stamp(due: Instant) -> String {
    let remaining = due.saturating_duration_since(Instant::now());
    (chrono::Utc::now() + chrono::Duration::from_std(remaining).unwrap_or_default())
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn configured_tasks(db: &Database, running: bool) -> Result<Vec<TaskSpec>, String> {
    let mut tasks = Vec::new();
    let store = DeviceStore::for_device();
    for app in [AppType::Claude, AppType::Codex] {
        let live = state::app_state(&store, app.as_str()).map_err(|e| e.to_string())?;
        let providers = db
            .get_all_providers(app.as_str())
            .map_err(|e| e.to_string())?;
        for (_, provider) in providers {
            let models: Vec<(String, ModelKeepAliveConfig, bool)> = match app {
                AppType::Claude => provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.stack_models.as_ref())
                    .into_iter()
                    .flatten()
                    .filter_map(|model| {
                        model
                            .keep_alive
                            .clone()
                            .map(|config| (model.model.trim().to_string(), config, true))
                    })
                    .collect(),
                AppType::Codex => provider
                    .settings_config
                    .pointer("/modelCatalog/models")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|model| {
                        let value = model.get("keepAlive").filter(|value| !value.is_null())?;
                        let parsed = serde_json::from_value::<ModelKeepAliveConfig>(value.clone());
                        let valid = parsed.is_ok();
                        let config = parsed.unwrap_or_else(|_| ModelKeepAliveConfig {
                            enabled: value
                                .get("enabled")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                            ..Default::default()
                        });
                        let name = model
                            .get("model")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .trim();
                        Some((name.to_string(), config, valid))
                    })
                    .collect(),
                _ => unreachable!(),
            };
            let mut seen = HashSet::new();
            for (model, config, parsed) in models {
                if !seen.insert(model.clone()) {
                    continue;
                }
                let pause_reason = if !parsed || config.validate().is_err() || model.is_empty() {
                    Some("invalid_config")
                } else if !running {
                    Some("proxy_stopped")
                } else if live.pending.is_some() {
                    Some("mode_changing")
                } else if live.mode != Some(state::Mode::Proxy)
                    || !live.attached
                    || !live.stack.enabled
                {
                    Some("not_aggregating")
                } else if !live.stack.is_member(&provider.id)
                    && live.proxy_route.as_deref() != Some(provider.id.as_str())
                {
                    Some("not_in_aggregation")
                } else if matches!(app, AppType::Codex)
                    && super::providers::is_codex_official_provider(&provider)
                {
                    // That route needs client-owned session/account headers. Never
                    // borrow another conversation's credentials for a probe.
                    Some("client_auth_required")
                } else {
                    None
                };
                tasks.push(TaskSpec {
                    app: app.clone(),
                    provider: provider.clone(),
                    model,
                    config,
                    pause_reason,
                });
            }
        }
    }
    Ok(tasks)
}

pub fn stopped_snapshot(
    db: &Database,
    app: &str,
    provider_id: &str,
) -> Result<Vec<ModelKeepAliveStatus>, String> {
    Ok(configured_tasks(db, false)?
        .into_iter()
        .filter(|spec| spec.app.as_str() == app && spec.provider.id == provider_id)
        .map(|spec| spec.status())
        .collect())
}

struct Worker {
    fingerprint: [u8; 32],
    handle: AbortHandle,
}

async fn stop_worker(tasks: &mut JoinSet<()>, worker: Worker) {
    worker.handle.abort();
    while !worker.handle.is_finished() {
        // Aborting schedules cancellation; await its completion before replacing
        // credentials/configuration so old and new generations never overlap.
        if tasks.join_next().await.is_none() {
            break;
        }
    }
}

/// Reconcile saved model rows every second. A shutdown waits for all aborted
/// requests to be dropped, including workers still queued on the global cap.
pub(super) async fn run(state: ProxyState, mut stop: watch::Receiver<bool>) {
    let runtime = state.keep_alive.clone();
    let mut workers: HashMap<TaskKey, Worker> = HashMap::new();
    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut config_read_failed = false;
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow_and_update() { break; }
            }
            _ = tick.tick() => {
                let specs = match configured_tasks(&state.db, true) {
                    Ok(specs) => { config_read_failed = false; specs }
                    Err(_) => {
                        if !config_read_failed {
                            log::warn!("[Model KeepAlive] Configuration unavailable; all background tasks paused");
                        }
                        config_read_failed = true;
                        workers.clear();
                        tasks.shutdown().await;
                        for status in runtime.statuses.lock().unwrap_or_else(|e| e.into_inner()).values() {
                            update(status, |s| { s.phase = "paused".into(); s.reason = Some("config_unavailable".into()); s.next_request_at = None; });
                        }
                        continue;
                    }
                };
                let keys: HashSet<_> = specs.iter().map(TaskSpec::key).collect();
                let removed: Vec<_> = workers.keys().filter(|key| !keys.contains(*key)).cloned().collect();
                for key in removed {
                    if let Some(worker) = workers.remove(&key) {
                        log::info!("[Model KeepAlive] App={} Provider={} Model={} Event=stop Reason=model_removed", key.0, key.1, key.2);
                        stop_worker(&mut tasks, worker).await;
                    }
                }
                runtime.statuses.lock().unwrap_or_else(|e| e.into_inner()).retain(|key, _| keys.contains(key));
                for spec in specs {
                    let key = spec.key();
                    let fingerprint = spec.fingerprint();
                    let enabled = spec.config.enabled && spec.pause_reason.is_none();
                    if enabled && workers.get(&key).is_some_and(|worker| worker.fingerprint == fingerprint && !worker.handle.is_finished()) {
                        continue;
                    }
                    if let Some(worker) = workers.remove(&key) {
                        log::info!("[Model KeepAlive] App={} Provider={} Model={} Event=stop Reason={}", key.0, key.1, key.2, if !spec.config.enabled { "disabled" } else { spec.pause_reason.unwrap_or("configuration_changed") });
                        stop_worker(&mut tasks, worker).await;
                    }
                    let status = {
                        let mut statuses = runtime.statuses.lock().unwrap_or_else(|e| e.into_inner());
                        if !enabled {
                            let status = statuses.entry(key.clone()).or_insert_with(|| Arc::new(Mutex::new(spec.status()))).clone();
                            update(&status, |s| {
                                s.config = spec.config.clone();
                                s.phase = if !spec.config.enabled { "disabled" } else if spec.pause_reason == Some("invalid_config") { "error" } else { "paused" }.into();
                                s.reason = spec.pause_reason.map(str::to_string);
                                s.next_request_at = None;
                            });
                            continue;
                        }
                        let status = Arc::new(Mutex::new(spec.status()));
                        statuses.insert(key.clone(), status.clone());
                        status
                    };
                    log::info!("[Model KeepAlive] App={} Provider={} Model={} Event=start Concurrency={} Interval={}s RecoveryInterval={}s Timeout={}s", spec.app.as_str(), spec.provider.id, spec.model, spec.config.recovery_concurrency, spec.config.interval_seconds, spec.config.retry_interval_seconds, spec.config.timeout_seconds);
                    let handle = tasks.spawn(run_model(state.clone(), spec, status));
                    workers.insert(key, Worker { fingerprint, handle });
                }
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    if !error.is_cancelled() { log::warn!("[Model KeepAlive] Worker ended unexpectedly; scheduler will restart it"); }
                }
            }
        }
    }
    tasks.shutdown().await;
    for status in runtime
        .statuses
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        update(status, |s| {
            s.active_requests = 0;
            s.next_request_at = None;
            s.phase = if s.config.enabled {
                "paused"
            } else {
                "disabled"
            }
            .into();
            s.reason = Some("proxy_stopped".into());
        });
    }
}

struct ActiveProbeGuard(SharedStatus);

impl Drop for ActiveProbeGuard {
    fn drop(&mut self) {
        update(&self.0, |status| {
            status.active_requests = status.active_requests.saturating_sub(1)
        });
    }
}

async fn wait_for_attempt(due: Instant) {
    if due <= Instant::now() {
        // Immediate retries still yield so foreground pauses and cancellation
        // can run even when a local failure completes without network I/O.
        tokio::task::yield_now().await;
    } else {
        sleep_until(due).await;
    }
}

async fn attempt(
    state: ProxyState,
    spec: TaskSpec,
    status: SharedStatus,
    due: Instant,
) -> Result<u16, probe::ProbeError> {
    wait_for_attempt(due).await;
    update(&status, |s| {
        if s.active_requests == 0 {
            s.phase = "queued".into();
            s.next_request_at = None;
        }
    });
    let _permit = state
        .keep_alive
        .slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| probe::ProbeError::new("scheduler_stopped", None))?;
    // Recheck after the capacity wait; foreground may have arrived while queued.
    let provider_key = (spec.app.as_str().to_string(), spec.provider.id.clone());
    if state
        .keep_alive
        .foreground
        .borrow()
        .get(&provider_key)
        .copied()
        .unwrap_or(0)
        > 0
    {
        return Err(probe::ProbeError::new("foreground_active", None));
    }
    // A permit may have been queued across a mode switch. Check the coherent
    // current state once more immediately before starting a network operation.
    let live = state::app_state(&DeviceStore::for_device(), spec.app.as_str())
        .map_err(|_| probe::ProbeError::new("configuration_error", None))?;
    if live.pending.is_some()
        || live.mode != Some(state::Mode::Proxy)
        || !live.attached
        || !live.stack.enabled
        || (!live.stack.is_member(&spec.provider.id)
            && live.proxy_route.as_deref() != Some(spec.provider.id.as_str()))
    {
        return Err(probe::ProbeError::new("mode_inactive", None));
    }
    let mut attempt_number = 0;
    update(&status, |s| {
        s.active_requests += 1;
        s.total_requests += 1;
        attempt_number = s.total_requests;
        s.last_attempt_at = Some(stamp());
        s.phase = "requesting".into();
        s.reason = None;
        s.next_request_at = None;
        log::info!(
            "[Model KeepAlive] App={} Provider={} Model={} Event=request Attempt={} InFlight={}",
            spec.app.as_str(),
            spec.provider.id,
            spec.model,
            s.total_requests,
            s.active_requests
        );
    });
    let _active = ActiveProbeGuard(status.clone());
    let response_meta = super::upstream_errors::UpstreamResponseMeta::default();
    let error_log = super::upstream_errors::UpstreamErrorContext::new(
        state.db.clone(), crate::database::UpstreamErrorSource::KeepAlive,
        spec.app.as_str(), &spec.provider, &spec.model, uuid::Uuid::new_v4().to_string(),
    );
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(spec.config.timeout_seconds as u64),
        probe::request(
            &state,
            &spec.app,
            &spec.provider,
            &spec.model,
            spec.config.timeout_seconds,
            spec.config.use_minimal_prompt,
            &status,
            &response_meta,
        ),
    )
    .await
    .unwrap_or_else(|_| Err(probe::ProbeError::new("timeout", None)));
    if let Err(error) = &result {
        error_log.record(error.http_status, error.code, error.detail.as_deref(),
            response_meta.request_id(), attempt_number, started.elapsed()).await;
    }
    result
}

async fn run_model(state: ProxyState, spec: TaskSpec, status: SharedStatus) {
    let mut foreground = state.keep_alive.foreground.subscribe();
    let provider_key = (spec.app.as_str().to_string(), spec.provider.id.clone());
    let mut keepalive = false;
    let mut consecutive_failures = 0u32;
    let mut due = Instant::now();
    let mut paused_recovery = false;
    'cycle: loop {
        let mut paused = false;
        while foreground
            .borrow_and_update()
            .get(&provider_key)
            .copied()
            .unwrap_or(0)
            > 0
        {
            paused = true;
            update(&status, |s| {
                s.phase = "paused".into();
                s.reason = Some("foreground_active".into());
                s.next_request_at = None;
            });
            if foreground.changed().await.is_err() { return; }
        }
        if paused {
            if keepalive {
                due = Instant::now() + Duration::from_secs(spec.config.interval_seconds as u64);
            } else {
                paused_recovery = true;
            }
        }
        let lane_count = if keepalive { 1 } else { spec.config.recovery_concurrency };
        let mut pending = FuturesUnordered::new();
        let mut scheduled = BTreeMap::new();
        let mut next_lane = 0u64;
        for _ in 0..lane_count {
            let lane_due = if paused_recovery {
                Instant::now() + spec.config.retry_delay.delay(spec.config.retry_interval_seconds, consecutive_failures.max(1))
            } else { due };
            let lane = next_lane;
            next_lane += 1;
            scheduled.insert(lane, lane_due);
            pending.push(recovery_attempt(state.clone(), spec.clone(), status.clone(), lane_due, lane, consecutive_failures));
        }
        paused_recovery = false;
        let next_due = scheduled.values().copied().min().unwrap_or(due);
        update(&status, |s| {
            s.mode = if keepalive { "keepalive" } else { "acquiring" }.into();
            s.phase = if next_due > Instant::now() {
                if keepalive { "waiting" } else { "retry_wait" }
            } else { "queued" }.into();
            s.reason = None;
            s.next_request_at = Some(due_stamp(next_due));
        });
        loop {
            tokio::select! {
                biased;
                changed = foreground.changed() => {
                    if changed.is_err() { return; }
                    if foreground.borrow_and_update().get(&provider_key).copied().unwrap_or(0) > 0 {
                        // Cancel both active requests and jittered sleepers on foreground work.
                        drop(pending);
                        paused_recovery = !keepalive;
                        continue 'cycle;
                    }
                }
                result = pending.next() => {
                    let Some((lane, lane_failures, result)) = result else { return; };
                    scheduled.remove(&lane);
                    match result {
                        Ok(http_status) => {
                            drop(pending);
                            keepalive = true;
                            consecutive_failures = 0;
                            due = Instant::now() + Duration::from_secs(spec.config.interval_seconds as u64);
                            update(&status, |s| {
                                s.mode = "keepalive".into();
                                s.phase = "waiting".into();
                                s.success_count += 1;
                                s.last_success_at = Some(stamp());
                                s.last_error = None;
                                s.last_http_status = Some(http_status);
                                s.next_request_at = Some(due_stamp(due));
                            });
                            log::info!("[Model KeepAlive] App={} Provider={} Model={} Event=success NextIn={}s", spec.app.as_str(), spec.provider.id, spec.model, spec.config.interval_seconds);
                            continue 'cycle;
                        }
                        Err(error) if error.code == "foreground_active" => {
                            drop(pending);
                            paused_recovery = !keepalive;
                            continue 'cycle;
                        }
                        Err(error) if error.code == "mode_inactive" => {
                            drop(pending);
                            update(&status, |s| { s.phase = "paused".into(); s.reason = Some("not_aggregating".into()); s.next_request_at = None; });
                            return;
                        }
                        Err(error) => {
                            let recovering = keepalive;
                            keepalive = false;
                            let failures = lane_failures.saturating_add(1);
                            consecutive_failures = consecutive_failures.max(failures);
                            // Each lane grows its own failure streak and samples independently.
                            // A failed successful-mode probe restores all recovery lanes.
                            let mut first_delay = None;
                            for _ in 0..if recovering { spec.config.recovery_concurrency } else { 1 } {
                                let delay = spec.config.retry_delay.delay(spec.config.retry_interval_seconds, failures);
                                first_delay.get_or_insert(delay);
                                let next = Instant::now() + delay;
                                let next_id = next_lane;
                                next_lane = next_lane.wrapping_add(1);
                                scheduled.insert(next_id, next);
                                pending.push(recovery_attempt(state.clone(), spec.clone(), status.clone(), next, next_id, failures));
                            }
                            let now = Instant::now();
                            let next_due = scheduled.values().copied().min().unwrap_or(now);
                            update(&status, |s| {
                                s.mode = "acquiring".into();
                                s.reason = None;
                                s.failure_count += 1;
                                s.last_error = Some(error.code.to_string());
                                s.last_http_status = error.http_status;
                                if s.active_requests == 0 {
                                    s.phase = if next_due <= now { "queued" } else { "retry_wait" }.into();
                                    s.next_request_at = Some(due_stamp(next_due));
                                }
                            });
                            log::info!("[Model KeepAlive] App={} Provider={} Model={} Event={} Status={:?} ErrorCode={} RetryMode={} ConsecutiveFailures={} RetryIn={:.3}s", spec.app.as_str(), spec.provider.id, spec.model, if recovering { "recover" } else { "retry" }, error.http_status, error.code, spec.config.retry_delay.mode.as_str(), failures, first_delay.unwrap_or_default().as_secs_f64());
                        }
                    }
                }
            }
        }
    }
}

async fn recovery_attempt(
    state: ProxyState, spec: TaskSpec, status: SharedStatus,
    due: Instant, lane: u64, failures: u32,
) -> (u64, u32, Result<u16, probe::ProbeError>) {
    (lane, failures, attempt(state, spec, status, due).await)
}

#[cfg(test)]
mod tests {
    use super::wait_for_attempt;
    use std::time::Duration;
    use tokio::time::Instant;

    #[tokio::test(start_paused = true)]
    async fn zero_retry_yields_without_adding_a_delay() {
        let start = Instant::now();
        let mut waiting = Box::pin(wait_for_attempt(start));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        waiting.await;
        assert_eq!(Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn positive_retry_waits_until_the_configured_deadline() {
        let start = Instant::now();
        let mut waiting = Box::pin(wait_for_attempt(start + Duration::from_secs(5)));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        waiting.await;
        assert!(Instant::now() - start >= Duration::from_secs(5));
    }
}
