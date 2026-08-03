//! Boundary ports used by the deterministic reconciler.

use crate::{CommitOid, Enrollment, ManagedRef, ReconcileResult, ReplayKey, RunId, Tips};
use async_trait::async_trait;

/// Sanitized boundary failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortErrorKind {
    /// Invalid/missing repository configuration, including a missing managed ref.
    Config,
    /// Credential or ACL rejection.
    Auth,
    /// Retryable transport or lock failure.
    Transient,
    /// Known verification failure.
    Verify,
    /// Unexpected adapter or cleanup failure.
    Unexpected,
}

/// Secret-free boundary error presented to the state machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("git-relay boundary failed ({kind:?})")]
pub struct PortError {
    kind: PortErrorKind,
}

impl PortError {
    /// Constructs a sanitized boundary error.
    pub const fn new(kind: PortErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable category.
    pub const fn kind(&self) -> PortErrorKind {
        self.kind
    }
}

/// Creates invocation IDs without coupling the core to a UUID implementation.
pub trait RunIdGenerator: Send + Sync {
    /// Returns the next unique invocation ID.
    fn next(&self) -> RunId;
}

/// Opens a scoped repository session for one enrollment.
#[async_trait]
pub trait RepositoryPort: Send + Sync {
    /// Opens a session whose credentials and repository identity were resolved
    /// by the host adapter.
    async fn open(&self, enrollment: &Enrollment) -> Result<Box<dyn RepositorySession>, PortError>;
}

/// Git operations whose ordering is owned by [`crate::Reconciler`].
#[async_trait]
pub trait RepositorySession: Send {
    /// Reads both managed tips.
    async fn tips(&mut self) -> Result<Tips, PortError>;
    /// Tests commit ancestry using the hydrated repository graph.
    async fn is_ancestor(
        &mut self,
        ancestor: &CommitOid,
        descendant: &CommitOid,
    ) -> Result<bool, PortError>;
    /// Re-reads only the Buzz managed tip for concurrent-change classification.
    async fn refresh_buzz(&mut self) -> Result<CommitOid, PortError>;
    /// Re-reads both tips for the final pre-mutation race check and verification.
    async fn refresh_both(&mut self) -> Result<Tips, PortError>;
    /// Pushes one exact source SHA to one exact destination ref.
    async fn push_exact(
        &mut self,
        target: &CommitOid,
        managed_ref: &ManagedRef,
    ) -> Result<(), PortError>;
    /// Closes and cleans up the repository session.
    async fn close(self: Box<Self>) -> Result<(), PortError>;
}

/// Acquires a per-enrollment/ref single-flight lock.
#[async_trait]
pub trait LockManager: Send + Sync {
    /// Acquires the lock or returns a sanitized boundary error.
    async fn acquire(&self, key: &str) -> Result<Box<dyn LockLease>, PortError>;
}

/// Owned lock lease released during reconciliation cleanup.
#[async_trait]
pub trait LockLease: Send {
    /// Releases the lease.
    async fn release(self: Box<Self>) -> Result<(), PortError>;
}

/// Append-only evidence and exact verified-delivery replay lookup.
#[async_trait]
pub trait EvidenceStore: Send + Sync {
    /// Finds a successful prior apply for the complete immutable replay key.
    async fn find_verified(&self, key: &ReplayKey) -> Result<Option<ReconcileResult>, PortError>;
    /// Appends current invocation evidence.
    async fn append(&self, result: &ReconcileResult) -> Result<(), PortError>;
}
