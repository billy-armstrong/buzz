use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// A validated domain value was malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {field}")]
pub struct ValidationError {
    field: &'static str,
}

macro_rules! validated_deserialize {
    ($name:ident) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

macro_rules! nonempty_id {
    ($name:ident, $field:literal, $docs:literal) => {
        #[doc = $docs]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validates and constructs the value.
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
                let value = value.into();
                if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
                    return Err(ValidationError { field: $field });
                }
                Ok(Self(value))
            }

            /// Returns the validated string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        validated_deserialize!($name);
    };
}

nonempty_id!(
    EnrollmentId,
    "enrollment_id",
    "Host-assigned enrollment identity."
);
nonempty_id!(
    GithubRepositoryId,
    "github_repository_id",
    "Immutable GitHub repository identity."
);
nonempty_id!(
    RunId,
    "run_id",
    "Unique reconciliation invocation identity."
);

/// A full Git object ID accepted by the v1 controller.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CommitOid(String);

impl CommitOid {
    /// Validates a lower-case, 40-character SHA-1 object ID.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.len() != 40
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ValidationError {
                field: "commit_oid",
            });
        }
        Ok(Self(value))
    }

    /// Returns the validated object ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

validated_deserialize!(CommitOid);

/// A validated 32-byte Nostr event ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ApprovalEventId(String);

impl ApprovalEventId {
    /// Validates a lower-case, 64-character hexadecimal event ID.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ValidationError {
                field: "approval_event_id",
            });
        }
        Ok(Self(value))
    }

    /// Returns the validated event ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

validated_deserialize!(ApprovalEventId);

/// The one exact destination ref managed by an enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ManagedRef(String);

impl ManagedRef {
    /// Constructs the initial supported managed ref.
    pub fn main() -> Self {
        Self("refs/heads/main".to_owned())
    }

    /// Validates the exact v1 managed ref allowlist.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value != "refs/heads/main" {
            return Err(ValidationError {
                field: "managed_ref",
            });
        }
        Ok(Self(value))
    }

    /// Returns the full destination ref.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

validated_deserialize!(ManagedRef);

/// Rollout gate for one enrolled repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutPhase {
    /// Observe only; mutation is disabled.
    ObserveOnly,
    /// Owner-approved exact fast-forwards may run.
    OwnerApprovedApply,
}

/// Explicit host-side enrollment for one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrollment {
    /// Host-assigned enrollment identity.
    pub id: EnrollmentId,
    /// Immutable GitHub repository identity.
    pub github_repository_id: GithubRepositoryId,
    /// The only destination ref this enrollment may manage.
    pub managed_ref: ManagedRef,
    /// Whether this repository is opted in.
    pub enabled: bool,
    /// Current rollout phase.
    pub rollout_phase: RolloutPhase,
    /// Independent host-side apply gate.
    pub apply_enabled: bool,
    /// Whether apply requires an owner approval event.
    pub approval_required: bool,
}

impl Enrollment {
    /// Creates an enabled observe-only enrollment.
    pub fn enabled(
        id: EnrollmentId,
        github_repository_id: GithubRepositoryId,
        managed_ref: ManagedRef,
    ) -> Self {
        Self {
            id,
            github_repository_id,
            managed_ref,
            enabled: true,
            rollout_phase: RolloutPhase::ObserveOnly,
            apply_enabled: false,
            approval_required: true,
        }
    }

    /// Enables owner-approved apply for testable host policy construction.
    pub fn with_owner_approved_apply(mut self) -> Self {
        self.rollout_phase = RolloutPhase::OwnerApprovedApply;
        self.apply_enabled = true;
        self
    }

    /// Marks the repository as unenrolled without discarding its identity.
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    pub(crate) fn policy_is_valid(&self) -> bool {
        self.approval_required
            && (self.rollout_phase != RolloutPhase::ObserveOnly || !self.apply_enabled)
    }
}

/// Reconciliation execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileMode {
    /// Classify and report only.
    Observe,
    /// Attempt an exact, approved fast-forward when proven safe.
    Apply,
}

/// Origin of a reconciliation invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileTrigger {
    /// Explicit owner/operator request.
    Manual,
    /// Scheduled deterministic audit.
    Audit,
    /// Future GitHub webhook adapter.
    GithubWebhook,
}

/// Input to the single reconciliation operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileRequest {
    /// Requested execution mode.
    pub mode: ReconcileMode,
    /// Invocation origin.
    pub trigger: ReconcileTrigger,
    /// Exact GitHub target approved by the owner, for apply only.
    pub expected_target: Option<CommitOid>,
    /// Owner approval event, for apply only.
    pub approval_event: Option<ApprovalEventId>,
}

impl ReconcileRequest {
    /// Creates the default manual observe request.
    pub fn observe() -> Self {
        Self {
            mode: ReconcileMode::Observe,
            trigger: ReconcileTrigger::Manual,
            expected_target: None,
            approval_event: None,
        }
    }

    /// Creates a manual apply request bound to an exact target and approval.
    pub fn apply(target: CommitOid, approval_event: ApprovalEventId) -> Self {
        Self {
            mode: ReconcileMode::Apply,
            trigger: ReconcileTrigger::Manual,
            expected_target: Some(target),
            approval_event: Some(approval_event),
        }
    }
}

/// The two authoritative tips read during reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tips {
    /// Current canonical GitHub tip.
    pub github: CommitOid,
    /// Current Buzz managed-ref tip.
    pub buzz: CommitOid,
}

/// Closed reconciliation state alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Repository is not opted in.
    Unmanaged,
    /// Enrollment or request policy is invalid.
    ConfigError,
    /// Both managed refs are exactly equal.
    InSync,
    /// Buzz is a strict ancestor of GitHub.
    RelayBehind,
    /// GitHub is a strict ancestor of Buzz.
    GithubBehind,
    /// Neither managed tip is an ancestor of the other.
    Diverged,
    /// Retryable boundary failure.
    TransientError,
    /// Credential or access failure.
    AuthError,
    /// Execution outcome could not be safely verified.
    VerifyError,
}

impl Classification {
    /// Stable machine spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmanaged => "unmanaged",
            Self::ConfigError => "config_error",
            Self::InSync => "in_sync",
            Self::RelayBehind => "relay_behind",
            Self::GithubBehind => "github_behind",
            Self::Diverged => "diverged",
            Self::TransientError => "transient_error",
            Self::AuthError => "auth_error",
            Self::VerifyError => "verify_error",
        }
    }
}

/// Truthful evidence about a possible Buzz ref mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationEvidence {
    /// No mutation was observed and no ambiguous push remains.
    None,
    /// Buzz was verified at the approved target.
    FastForward,
    /// A push was attempted but mutation attribution is not provable.
    Unknown,
}

impl MutationEvidence {
    /// Stable machine spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::FastForward => "fast_forward",
            Self::Unknown => "unknown",
        }
    }
}

/// Stable process-level outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitOutcome {
    /// Reconciliation reached its requested safe state.
    Success,
    /// Reconciliation froze or failed and requires attention/retry.
    Failure,
}

/// Fixed-schema, secret-free reconciliation evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileResult {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Current invocation ID.
    pub run_id: RunId,
    /// Enrollment identity.
    pub enrollment_id: EnrollmentId,
    /// Immutable GitHub repository identity.
    pub github_repository_id: GithubRepositoryId,
    /// Exact managed destination ref.
    pub managed_ref: ManagedRef,
    /// Requested mode.
    pub mode: ReconcileMode,
    /// Invocation origin.
    pub trigger: ReconcileTrigger,
    /// Final classification.
    pub classification: Classification,
    /// Initial GitHub tip, when read.
    pub github_before: Option<CommitOid>,
    /// Initial Buzz tip, when read.
    pub buzz_before: Option<CommitOid>,
    /// Current/approved GitHub target, when known.
    pub github_target: Option<CommitOid>,
    /// Trusted Buzz after-tip, when verified.
    pub buzz_after: Option<CommitOid>,
    /// Mutation evidence.
    pub mutation: MutationEvidence,
    /// Whether the controller reached the push seam; `None` is reserved for
    /// outer adapters that cannot determine entry into the core.
    pub push_attempted: Option<bool>,
    /// Approval event bound to the invocation.
    pub approval_event: Option<ApprovalEventId>,
    /// Whether deterministic retry is appropriate.
    pub retryable: bool,
    /// Stable operator action token.
    pub next_action: String,
    /// Sanitized human summary.
    pub summary: String,
    /// Stable process outcome.
    pub outcome: ExitOutcome,
    /// Conventional machine exit code.
    pub exit_code: u8,
}

/// Immutable key for a previously verified apply delivery.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplayKey {
    /// Enrollment identity.
    pub enrollment_id: EnrollmentId,
    /// Immutable repository identity prevents state reuse after repointing.
    pub github_repository_id: GithubRepositoryId,
    /// Exact destination ref.
    pub managed_ref: ManagedRef,
    /// Exact delivered target.
    pub target: CommitOid,
    /// Exact owner approval event.
    pub approval_event: ApprovalEventId,
}
