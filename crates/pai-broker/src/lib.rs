//! Compute broker — "where should this workload run?"
//!
//! V1 implements the *policy* honestly: local-first placement over known
//! device capabilities. Intelligent scheduling (latency, thermal, cost)
//! slots in behind the same `route` signature later.

use pai_core::*;
use serde::{Deserialize, Serialize};

pub mod rpc;

/// A unit of work the broker can place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workload {
    /// Required capability, e.g. TextGeneration, ImageGeneration.
    pub capability: ModelCapability,
    /// Rough resource need, if known.
    pub estimated_ram_bytes: Option<u64>,
    /// True if the request must not leave the device (privacy / offline).
    pub requires_local: bool,
}

/// Where the broker decided to run a workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Placement {
    LocalDevice,
    TrustedDevice {
        device: DeviceId,
    },
    /// Only reachable when policy allows cloud.
    RemoteProvider {
        provider: String,
    },
    /// Nothing can satisfy it under current policy.
    Unavailable {
        reason: String,
    },
}

pub struct ComputeBroker {
    policy: pai_config::ComputePolicy,
    /// All devices the user owns, with live-ish capabilities.
    devices: Vec<Device>,
    /// This device.
    local: DeviceId,
}

impl ComputeBroker {
    pub fn new(policy: pai_config::ComputePolicy, devices: Vec<Device>, local: DeviceId) -> Self {
        Self {
            policy,
            devices,
            local,
        }
    }

    /// Choose where `w` should execute under the configured policy.
    pub fn route(&self, w: &Workload) -> Placement {
        use pai_config::ComputePolicy::*;
        let local = self.devices.iter().find(|d| d.id == self.local);
        let remote_peers: Vec<&Device> =
            self.devices.iter().filter(|d| d.id != self.local).collect();

        let can = |d: &Device| -> bool {
            d.capabilities
                .supported_capabilities
                .contains(&w.capability)
                && w.estimated_ram_bytes
                    .map(|need| d.capabilities.ram_bytes >= need)
                    .unwrap_or(true)
        };

        // 1. Local device first — always.
        if let Some(d) = local {
            if can(d) {
                return Placement::LocalDevice;
            }
        }
        if w.requires_local || self.policy == LocalOnly {
            return Placement::Unavailable {
                reason: "requires local execution; local device cannot serve it".into(),
            };
        }

        // 2. Trusted local-network devices, if policy permits.
        if matches!(
            self.policy,
            LocalPreferred | TrustedLocalNetwork | CloudAllowed | CloudPreferred
        ) {
            if let Some(d) = remote_peers.iter().find(|d| can(d)) {
                return Placement::TrustedDevice { device: d.id };
            }
        }

        // 3. Cloud only when the user opted in.
        if matches!(self.policy, CloudAllowed | CloudPreferred) {
            return Placement::RemoteProvider {
                provider: "configured-remote".into(),
            };
        }

        Placement::Unavailable {
            reason: "no capable device and cloud disallowed by policy".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pai_config::ComputePolicy;

    fn device(id: DeviceId, caps: Vec<ModelCapability>, ram: u64) -> Device {
        Device {
            id,
            owner: UserId::new(),
            name: "d".into(),
            platform: Platform::Linux,
            public_key: vec![],
            registered_at: now(),
            last_seen_at: now(),
            capabilities: DeviceCapabilities {
                ram_bytes: ram,
                supported_capabilities: caps,
                ..Default::default()
            },
        }
    }

    #[test]
    fn local_first_then_trusted_then_unavailable() {
        let local_id = DeviceId::new();
        let peer_id = DeviceId::new();
        let w = Workload {
            capability: ModelCapability::ImageGeneration,
            estimated_ram_bytes: None,
            requires_local: false,
        };
        // Local can't, peer can → trusted device under LOCAL_PREFERRED.
        let broker = ComputeBroker::new(
            ComputePolicy::LocalPreferred,
            vec![
                device(local_id, vec![ModelCapability::TextGeneration], 8 << 30),
                device(peer_id, vec![ModelCapability::ImageGeneration], 32 << 30),
            ],
            local_id,
        );
        assert_eq!(
            broker.route(&w),
            Placement::TrustedDevice { device: peer_id }
        );
        // LOCAL_ONLY → unavailable.
        let broker = ComputeBroker::new(ComputePolicy::LocalOnly, broker.devices.clone(), local_id);
        assert!(matches!(broker.route(&w), Placement::Unavailable { .. }));
        // Local can → local wins even when peer exists.
        let w2 = Workload {
            capability: ModelCapability::TextGeneration,
            estimated_ram_bytes: None,
            requires_local: false,
        };
        assert_eq!(broker.route(&w2), Placement::LocalDevice);
    }
}
