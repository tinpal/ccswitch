//! Session-scoped policies learned from accepted Azure resource-conflict repairs.
//!
//! Requests retain their own retry budget. Only the cleanup policy is shared, and
//! only after a repaired request is accepted. Keys contain hashes, never credentials
//! or conversation content. An idle policy expires and the store stays bounded.

use crate::provider::Provider;
use crate::proxy::{
    json_canonical::canonical_json_string, opaque_state_rectifier::OpaqueStateRejection,
};
use http::HeaderMap;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_SESSION_POLICIES: usize = 1024;
const POLICY_IDLE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

// Effective authentication can come from the provider, a managed account, the
// client, or header overrides. Volatile request/turn headers must not change the key.
const IDENTITY_HEADERS: &[&str] = &[
    "authorization",
    "api-key",
    "x-api-key",
    "chatgpt-account-id",
    "openai-organization",
    "openai-project",
    "x-account-id",
    "x-tenant-id",
    "x-resource-id",
    "x-ms-resource-id",
];

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::proxy) struct OpaqueStateScope {
    digest: [u8; 32],
    client_provided: bool,
}

impl OpaqueStateScope {
    pub(in crate::proxy) fn new(
        session_id: &str,
        client_provided: bool,
        provider: &Provider,
        model: &str,
        url: &str,
        headers: &HeaderMap,
    ) -> Self {
        let mut digest = Sha256::new();
        for value in [
            "cc-switch/native-responses/resource-policy/v1",
            session_id,
            &provider.id,
            model,
            url,
        ] {
            hash_field(&mut digest, value.as_bytes());
        }
        // Include configured credentials/custom identity headers as well as the
        // final wire authentication. Changing a provider's identity isolates it.
        hash_field(
            &mut digest,
            canonical_json_string(&provider.settings_config).as_bytes(),
        );
        if let Some(overrides) = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.local_proxy_request_overrides.as_ref())
        {
            if let Ok(value) = serde_json::to_value(overrides) {
                hash_field(&mut digest, canonical_json_string(&value).as_bytes());
            }
        }
        for name in IDENTITY_HEADERS {
            hash_field(&mut digest, name.as_bytes());
            let values = headers.get_all(*name);
            hash_field(&mut digest, &(values.iter().count() as u64).to_le_bytes());
            for value in values {
                hash_field(&mut digest, value.as_bytes());
            }
        }
        Self {
            digest: digest.finalize().into(),
            client_provided: client_provided && !session_id.trim().is_empty(),
        }
    }

    pub(super) fn load(self) -> Option<OpaqueStateRejection> {
        // Generated session IDs are suitable for request-local isolation only.
        if !self.client_provided {
            return None;
        }
        let mut policies = policies().lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let policy = policies.get_mut(&self)?;
        if now.duration_since(policy.last_used) >= POLICY_IDLE_TTL {
            policies.remove(&self);
            return None;
        }
        policy.last_used = now;
        Some(policy.rejection)
    }

    pub(super) fn remember(self, rejection: OpaqueStateRejection) {
        // Unknown compaction types/invalid ciphertext alone do not prove that a
        // gateway switches resources. Keep those compatibility fixes request-local.
        if !self.client_provided || !rejection.item_ids {
            return;
        }
        let mut policies = policies().lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        policies.retain(|_, policy| now.duration_since(policy.last_used) < POLICY_IDLE_TTL);
        if let Some(policy) = policies.get_mut(&self) {
            policy.rejection = policy.rejection.merge(rejection);
            policy.last_used = now;
            return;
        }
        if policies.len() >= MAX_SESSION_POLICIES {
            if let Some(oldest) = policies
                .iter()
                .min_by_key(|(_, policy)| policy.last_used)
                .map(|(scope, _)| *scope)
            {
                policies.remove(&oldest);
            }
        }
        policies.insert(
            self,
            SessionPolicy {
                rejection,
                last_used: now,
            },
        );
    }
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

struct SessionPolicy {
    rejection: OpaqueStateRejection,
    last_used: Instant,
}

fn policies() -> &'static Mutex<HashMap<OpaqueStateScope, SessionPolicy>> {
    static POLICIES: OnceLock<Mutex<HashMap<OpaqueStateScope, SessionPolicy>>> = OnceLock::new();
    POLICIES.get_or_init(|| Mutex::new(HashMap::new()))
}
