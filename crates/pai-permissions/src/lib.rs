//! Explicit permission system — the platform's critical safety boundary.
//!
//! Every tool declares the permissions it needs. Every execution passes
//! through [`PolicyEngine::decide`]. A model can never widen its own access;
//! policies are platform state, not model output.

use pai_core::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single auditable capability. Declared per-resource, per-verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Permission {
    EmailRead,
    EmailSearch,
    EmailLabel,
    EmailArchive,
    EmailDelete,
    EmailDraft,
    EmailSend,
    CalendarRead,
    CalendarCreate,
    CalendarUpdate,
    CalendarDelete,
    FilesRead,
    FilesWrite,
    FilesDelete,
    MicrophoneAccess,
    CameraAccess,
    ContactsRead,
    MemoryWrite,
    MemoryRead,
    MemoryDelete,
    DocumentRead,
    DocumentWrite,
    ComputeLocal,
    ComputeTrustedDevice,
    ComputeCloud,
    /// Publish to the notification inbox (+ configured external channels).
    NotificationSend,
}

/// The engine's verdict for one action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PermissionDecision {
    Allow,
    /// Blocked until the user approves/denies — the run suspends.
    AskUser,
    Deny,
}

/// A user-editable rule: `(permission) → policy`. Persisted by the app;
/// the model cannot write to this map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyTable {
    rules: BTreeMap<Permission, ExecutionPolicy>,
}

impl PolicyTable {
    /// Conservative defaults — the platform ships least-privilege.
    pub fn with_defaults() -> Self {
        use ExecutionPolicy::*;
        use Permission::*;
        let mut t = Self::default();
        for (p, pol) in [
            (EmailRead, AlwaysAllow),
            (EmailSearch, AlwaysAllow),
            (EmailDraft, AlwaysAllow),
            (EmailLabel, AskUser),
            (EmailArchive, AskUser),
            (EmailSend, AskUser),
            (EmailDelete, AskUser),
            (CalendarRead, AlwaysAllow),
            (CalendarCreate, AskUser),
            (CalendarUpdate, AskUser),
            (CalendarDelete, AskUser),
            (FilesRead, AlwaysAllow),
            (FilesWrite, AskUser),
            (FilesDelete, AskUser),
            (MicrophoneAccess, AskUser),
            (CameraAccess, AskUser),
            (ContactsRead, AskUser),
            (MemoryWrite, AlwaysAllow),
            (MemoryRead, AlwaysAllow),
            (MemoryDelete, AskUser),
            (DocumentRead, AlwaysAllow),
            (DocumentWrite, AlwaysAllow),
            (ComputeLocal, AlwaysAllow),
            (ComputeTrustedDevice, AskUser),
            (ComputeCloud, AskUser),
            // Inbox is local + reversible; external channels only fire
            // when the user configured them in notify.json.
            (NotificationSend, AlwaysAllow),
        ] {
            t.set(p, pol);
        }
        t
    }

    pub fn set(&mut self, p: Permission, policy: ExecutionPolicy) {
        self.rules.insert(p, policy);
    }

    pub fn get(&self, p: Permission) -> Option<ExecutionPolicy> {
        self.rules.get(&p).copied()
    }

    /// Iterate the full rule set (for the policy editor / serialization).
    pub fn iter(&self) -> impl Iterator<Item = (Permission, ExecutionPolicy)> + '_ {
        self.rules.iter().map(|(p, pol)| (*p, *pol))
    }
}

/// All permissions the platform knows about — drives the policy editor UI.
pub fn all_permissions() -> Vec<Permission> {
    use Permission::*;
    vec![
        EmailRead,
        EmailSearch,
        EmailLabel,
        EmailArchive,
        EmailDelete,
        EmailDraft,
        EmailSend,
        CalendarRead,
        CalendarCreate,
        CalendarUpdate,
        CalendarDelete,
        FilesRead,
        FilesWrite,
        FilesDelete,
        MicrophoneAccess,
        CameraAccess,
        ContactsRead,
        MemoryWrite,
        MemoryRead,
        MemoryDelete,
        DocumentRead,
        DocumentWrite,
        ComputeLocal,
        ComputeTrustedDevice,
        ComputeCloud,
    ]
}

/// Live policy engine. The table is mutable so the app can apply user
/// policy changes without rebuilding the runtime; callers persist edits.
pub struct PolicyEngine {
    table: std::sync::RwLock<PolicyTable>,
}

impl PolicyEngine {
    pub fn new(table: PolicyTable) -> Self {
        Self {
            table: std::sync::RwLock::new(table),
        }
    }

    /// Decide whether `permissions` may run. Multiple required permissions
    /// resolve to the *most restrictive* verdict.
    pub fn decide(&self, permissions: &[Permission]) -> PermissionDecision {
        let table = self.table.read().unwrap();
        let mut decision = PermissionDecision::Allow;
        for p in permissions {
            match table.get(*p).unwrap_or(ExecutionPolicy::AskUser) {
                ExecutionPolicy::NeverAllow => return PermissionDecision::Deny,
                ExecutionPolicy::AskUser => decision = PermissionDecision::AskUser,
                ExecutionPolicy::AllowWithRule | ExecutionPolicy::AlwaysAllow => {}
            }
        }
        decision
    }

    /// Apply a user policy edit in memory. Persistence is the caller's job.
    pub fn set_policy(&self, p: Permission, policy: ExecutionPolicy) {
        self.table.write().unwrap().set(p, policy);
    }

    /// Effective policy for one permission (default when no rule exists).
    pub fn effective(&self, p: Permission) -> ExecutionPolicy {
        self.table
            .read()
            .unwrap()
            .get(p)
            .unwrap_or(ExecutionPolicy::AskUser)
    }

    /// Snapshot of all rules, for serialization / the policy editor.
    pub fn rules(&self) -> Vec<(Permission, ExecutionPolicy)> {
        self.table.read().unwrap().iter().collect()
    }
}

/// One pending approval surfaced to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: ToolCallId,
    pub tool: String,
    pub permissions: Vec<Permission>,
    /// Human-readable summary, e.g. "Send email to sarah@x.com".
    pub summary: String,
    pub risk: RiskLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_least_privilege() {
        let engine = PolicyEngine::new(PolicyTable::with_defaults());
        assert_eq!(
            engine.decide(&[Permission::FilesRead]),
            PermissionDecision::Allow
        );
        assert_eq!(
            engine.decide(&[Permission::EmailSend]),
            PermissionDecision::AskUser
        );
        // Most-restrictive wins.
        assert_eq!(
            engine.decide(&[Permission::FilesRead, Permission::EmailDelete]),
            PermissionDecision::AskUser
        );
        let mut t = PolicyTable::with_defaults();
        t.set(Permission::EmailDelete, ExecutionPolicy::NeverAllow);
        assert_eq!(
            PolicyEngine::new(t).decide(&[Permission::EmailDelete]),
            PermissionDecision::Deny
        );
    }
}
