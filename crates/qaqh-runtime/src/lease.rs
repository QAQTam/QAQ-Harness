use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const LEASE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseDecision {
    Acquired,
    Resumed,
    Denied {
        owner_kind: String,
        retry_after_ms: u64,
    },
}

struct Lease {
    client_instance_id: String,
    client_kind: String,
    connection_id: String,
    expires_at: Instant,
}

#[derive(Default)]
pub struct LeaseManager {
    leases: HashMap<String, Lease>,
}

impl LeaseManager {
    pub fn attach(
        &mut self,
        seed: &str,
        client_instance_id: &str,
        client_kind: &str,
        connection_id: &str,
        now: Instant,
    ) -> LeaseDecision {
        self.expire(now);
        if let Some(existing) = self.leases.get_mut(seed) {
            if existing.client_instance_id == client_instance_id {
                existing.connection_id = connection_id.to_string();
                existing.expires_at = now + LEASE_TIMEOUT;
                return LeaseDecision::Resumed;
            }
            return LeaseDecision::Denied {
                owner_kind: existing.client_kind.clone(),
                retry_after_ms: existing
                    .expires_at
                    .saturating_duration_since(now)
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            };
        }
        self.leases.insert(
            seed.to_string(),
            Lease {
                client_instance_id: client_instance_id.to_string(),
                client_kind: client_kind.to_string(),
                connection_id: connection_id.to_string(),
                expires_at: now + LEASE_TIMEOUT,
            },
        );
        LeaseDecision::Acquired
    }

    pub fn renew_connection(&mut self, connection_id: &str, now: Instant) {
        for lease in self.leases.values_mut() {
            if lease.connection_id == connection_id {
                lease.expires_at = now + LEASE_TIMEOUT;
            }
        }
        self.expire(now);
    }

    pub fn detach(&mut self, seed: &str, client_instance_id: &str) -> bool {
        if self
            .leases
            .get(seed)
            .is_some_and(|lease| lease.client_instance_id == client_instance_id)
        {
            self.leases.remove(seed);
            true
        } else {
            false
        }
    }

    pub fn owns(&mut self, seed: &str, client_instance_id: &str, now: Instant) -> bool {
        self.expire(now);
        self.leases
            .get(seed)
            .is_some_and(|lease| lease.client_instance_id == client_instance_id)
    }

    pub fn attached_for(&mut self, client_instance_id: &str, now: Instant) -> Vec<String> {
        self.expire(now);
        self.leases
            .iter()
            .filter(|(_, lease)| lease.client_instance_id == client_instance_id)
            .map(|(seed, _)| seed.clone())
            .collect()
    }

    fn expire(&mut self, now: Instant) {
        self.leases.retain(|_, lease| lease.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_client_resumes_and_other_client_waits_for_expiry() {
        let now = Instant::now();
        let mut leases = LeaseManager::default();
        assert_eq!(
            leases.attach("s", "a", "desktop", "c1", now),
            LeaseDecision::Acquired
        );
        assert_eq!(
            leases.attach("s", "a", "desktop", "c2", now),
            LeaseDecision::Resumed
        );
        assert!(matches!(
            leases.attach("s", "b", "tui", "c3", now),
            LeaseDecision::Denied { .. }
        ));
        assert_eq!(
            leases.attach("s", "b", "tui", "c3", now + LEASE_TIMEOUT),
            LeaseDecision::Acquired
        );
    }

    #[test]
    fn only_the_owner_can_control_or_detach_a_lease() {
        let now = Instant::now();
        let mut leases = LeaseManager::default();
        leases.attach("s", "owner", "desktop", "c1", now);
        assert!(leases.owns("s", "owner", now));
        assert!(!leases.owns("s", "other", now));
        assert!(!leases.detach("s", "other"));
        assert!(leases.detach("s", "owner"));
        assert!(!leases.owns("s", "owner", now));
    }

    #[test]
    fn heartbeat_renews_only_the_matching_connection() {
        let now = Instant::now();
        let mut leases = LeaseManager::default();
        leases.attach("a", "one", "desktop", "c1", now);
        leases.attach("b", "two", "tui", "c2", now);
        leases.renew_connection("c1", now + Duration::from_secs(10));
        assert!(leases.owns("a", "one", now + Duration::from_secs(16)));
        assert!(!leases.owns("b", "two", now + Duration::from_secs(16)));
    }
}
