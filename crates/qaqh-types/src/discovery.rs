//! Daemon discovery contract（磁盘 `daemon.json` 的唯一权威定义）。
//!
//! PR-3-1 单源化：此前 proto 侧 control 模块与 client 侧 discovery
//! 各持一份手工副本，磁盘契约被定义两次、零编译护栏；现统一收敛于
//! `qaqh-types`（daemon 写入、client/TUI/shell 读取均引用本定义）。

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
}
