//! Deterministic GitHub-to-Buzz repository reconciliation.
//!
//! This crate owns the reconciliation state machine and its safety ordering.
//! Network, persistence, Nostr, UI, and process concerns enter only through
//! the boundary traits in [`ports`].

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod domain;
pub mod ports;
mod reconciler;

pub use domain::{
    ApprovalEventId, Classification, CommitOid, Enrollment, EnrollmentId, ExitOutcome,
    GithubRepositoryId, ManagedRef, MutationEvidence, NextAction, ReconcileMode, ReconcileRequest,
    ReconcileResult, ReconcileTrigger, ReplayKey, RolloutPhase, RunId, Tips, ValidationError,
};
pub use ports::{PortError, PortErrorKind, RunIdGenerator};
pub use reconciler::Reconciler;
