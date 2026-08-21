//! Daemon discovery contract.
//!
//! The legacy WebSocket control protocol (`ControlClientMessage` /
//! `ControlServerMessage` / `ControlSnapshot`) was removed with the
//! Ringing HTTP/SSE migration. What remains is the on-disk discovery
//! record that lets clients locate and authenticate the daemon.

use serde::{Deserialize, Serialize};

pub const CONTROL_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDiscovery {
    pub endpoint: String,
    pub token: String,
    pub pid: u32,
    pub server_epoch: String,
    pub protocol_version: u16,
    /// Semver of the daemon binary. Defaults keep pre-0.9 discovery files
    /// readable so clients can replace them gracefully.
    #[serde(default)]
    pub daemon_version: String,
    /// Source/build identity used to reject a stale daemon with a compatible
    /// wire protocol.
    #[serde(default)]
    pub build_id: String,
    /// Runtime lane (for example `stable` or `dev`). Development clients must
    /// never silently adopt the installed stable daemon, or vice versa.
    #[serde(default)]
    pub channel: String,
    /// Canonical executable path, primarily for diagnostics and ownership
    /// checks during upgrades.
    #[serde(default)]
    pub executable: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_round_trip_preserves_protocol_version() {
        let discovery = DaemonDiscovery {
            endpoint: "ws://127.0.0.1:42/control/v1".into(),
            token: "secret".into(),
            pid: 7,
            server_epoch: "epoch".into(),
            protocol_version: CONTROL_PROTOCOL_VERSION,
            daemon_version: "0.9.0".into(),
            build_id: "commit".into(),
            channel: "stable".into(),
            executable: "/opt/qaqh/qaqh-daemon".into(),
        };
        let json = serde_json::to_string(&discovery).unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonDiscovery>(&json).unwrap(),
            discovery
        );
    }

    #[test]
    fn discovery_defaults_identity_fields() {
        let discovery: DaemonDiscovery = serde_json::from_value(serde_json::json!({
            "endpoint": "ws://127.0.0.1:42/control/v1",
            "token": "secret",
            "pid": 7,
            "server_epoch": "epoch",
            "protocol_version": CONTROL_PROTOCOL_VERSION
        }))
        .unwrap();
        assert!(discovery.daemon_version.is_empty());
        assert!(discovery.build_id.is_empty());
        assert!(discovery.channel.is_empty());
        assert!(discovery.executable.is_empty());
    }
}
