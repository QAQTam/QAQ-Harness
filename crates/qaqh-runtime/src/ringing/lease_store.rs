//! Ringing client session lease store.
//!
//! Extracted from `qaqh-daemon/src/ringing_http.rs:2866` for sharing between
//! the legacy hand-written TCP path and the new axum path. Single source of
//! truth; no drift.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const RENEW_TTL_MS: u64 = 30_000;

fn lease_ttl_ms() -> u64 {
    std::env::var("QAQH_TEST_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENEW_TTL_MS)
}

/// Ringing 逻辑 client session lease.
///
/// 键 = 客户端自生成的 `client_instance_id`；值记录服务端签发的
/// `client_session_id`，后续请求必须通过 header 携带该 session id。
/// open 时双 id 关联，renew 按 client_session_id 反查续期。
#[derive(Debug, Default)]
pub struct RingingLeaseStore {
    leases: HashMap<String, LeaseEntry>,
    seed_leases: HashMap<String, HashSet<String>>,
}

#[derive(Debug, Clone)]
struct LeaseEntry {
    client_session_id: String,
    expiry: Instant,
}

impl RingingLeaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self, client_session_id: String, client_instance_id: String) {
        self.leases.insert(
            client_instance_id,
            LeaseEntry {
                client_session_id,
                expiry: Instant::now() + Duration::from_millis(lease_ttl_ms()),
            },
        );
    }

    pub fn attach_seed(&mut self, client_session_id: &str, seed: &str) -> bool {
        if !self.is_active_session(client_session_id) || seed.is_empty() {
            return false;
        }
        self.seed_leases
            .entry(client_session_id.to_string())
            .or_default()
            .insert(seed.to_string());
        true
    }

    pub fn detach_seed(&mut self, client_session_id: &str, seed: &str) {
        if let Some(seeds) = self.seed_leases.get_mut(client_session_id) {
            seeds.remove(seed);
            if seeds.is_empty() {
                self.seed_leases.remove(client_session_id);
            }
        }
    }

    pub fn owns_seed(&mut self, client_session_id: &str, seed: &str) -> bool {
        self.expire();
        self.seed_leases
            .get(client_session_id)
            .is_some_and(|seeds| seeds.contains(seed))
    }

    /// 续租（按 client_session_id 反查）；过期/未知会话返回 false。
    pub fn renew(&mut self, client_session_id: &str) -> bool {
        let Some(entry) = self
            .leases
            .values_mut()
            .find(|e| e.client_session_id == client_session_id)
        else {
            return false;
        };
        if entry.expiry < Instant::now() {
            let victim = self
                .leases
                .iter()
                .find(|(_, e)| e.client_session_id == client_session_id)
                .map(|(k, _)| k.clone());
            if let Some(k) = victim {
                if let Some(entry) = self.leases.remove(&k) {
                    self.seed_leases.remove(&entry.client_session_id);
                }
            }
            return false;
        }
        entry.expiry = Instant::now() + Duration::from_millis(lease_ttl_ms());
        true
    }

    fn expire(&mut self) {
        let expired: HashSet<String> = self
            .leases
            .iter()
            .filter(|(_, entry)| entry.expiry < Instant::now())
            .map(|(instance, _)| instance.clone())
            .collect();
        for instance in expired {
            if let Some(entry) = self.leases.remove(&instance) {
                self.seed_leases.remove(&entry.client_session_id);
            }
        }
    }

    /// 活跃校验（按 client_instance_id；命令/切流端点使用）
    pub fn is_active(&self, client_instance_id: &str) -> bool {
        self.leases
            .get(client_instance_id)
            .is_some_and(|e| e.expiry >= Instant::now())
    }

    pub fn is_active_session(&self, client_session_id: &str) -> bool {
        self.leases.values().any(|entry| {
            entry.client_session_id == client_session_id && entry.expiry >= Instant::now()
        })
    }

    pub fn instance_for_session(&self, client_session_id: &str) -> Option<String> {
        self.leases.iter().find_map(|(instance, entry)| {
            (entry.client_session_id == client_session_id && entry.expiry >= Instant::now())
                .then(|| instance.clone())
        })
    }

    /// Test helper: force expiry for a given instance id.
    pub fn set_expiry_for_test(&mut self, client_instance_id: &str, expiry: Instant) {
        if let Some(entry) = self.leases.get_mut(client_instance_id) {
            entry.expiry = expiry;
        }
    }
}
