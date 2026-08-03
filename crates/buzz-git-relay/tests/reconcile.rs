use buzz_git_relay::{
    ports::{EvidenceStore, LockLease, LockManager, RepositoryPort, RepositorySession},
    ApprovalEventId, Classification, CommitOid, Enrollment, EnrollmentId, GithubRepositoryId,
    ManagedRef, MutationEvidence, NextAction, PortError, PortErrorKind, ReconcileRequest,
    ReconcileResult, Reconciler, ReplayKey, RunId, RunIdGenerator, Tips,
};
use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const APPROVAL: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const OTHER_APPROVAL: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

#[tokio::test]
async fn equal_tips_are_in_sync_without_mutation() {
    let fixture = Fixture::new(SHA_A, SHA_A);
    let result = fixture.observe().await;
    assert_result(&result, Classification::InSync, MutationEvidence::None, 0);
    assert_eq!(fixture.state().pushes.len(), 0);
}

#[tokio::test]
async fn observe_reports_relay_behind_without_mutation() {
    let fixture = Fixture::new(SHA_B, SHA_A).ancestor(SHA_A, SHA_B);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::RelayBehind,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.next_action, NextAction::ApproveExactFastForward);
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn approved_apply_fast_forwards_exact_ref_and_verifies() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::InSync,
        MutationEvidence::FastForward,
        0,
    );
    assert_eq!(
        fixture.state().pushes,
        vec![(SHA_B.to_owned(), "refs/heads/main".to_owned())]
    );
}

#[tokio::test]
async fn github_behind_freezes_without_mutation() {
    let fixture = Fixture::new(SHA_A, SHA_B).ancestor(SHA_A, SHA_B);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::GithubBehind,
        MutationEvidence::None,
        1,
    );
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn diverged_histories_freeze_without_mutation() {
    let fixture = Fixture::new(SHA_A, SHA_B);
    let result = fixture.observe().await;
    assert_result(&result, Classification::Diverged, MutationEvidence::None, 1);
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn invalid_enrollment_policy_returns_config_error() {
    let mut fixture = Fixture::new(SHA_A, SHA_A);
    fixture.enrollment.approval_required = false;
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(fixture.state().open_calls, 0);
}

#[tokio::test]
async fn disabled_enrollment_is_unmanaged_and_never_opens_repository() {
    let mut fixture = Fixture::new(SHA_A, SHA_A);
    fixture.enrollment = fixture.enrollment.clone().disabled();
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::Unmanaged,
        MutationEvidence::None,
        1,
    );
    assert_eq!(fixture.state().open_calls, 0);
}

#[tokio::test]
async fn authentication_failure_freezes_without_retry() {
    let fixture = Fixture::new(SHA_A, SHA_A).open_error(PortErrorKind::Auth);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::AuthError,
        MutationEvidence::None,
        1,
    );
    assert!(!result.retryable);
}

#[tokio::test]
async fn temporary_remote_failure_is_retryable_without_mutation() {
    let fixture = Fixture::new(SHA_A, SHA_A).open_error(PortErrorKind::Transient);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::TransientError,
        MutationEvidence::None,
        1,
    );
    assert!(result.retryable);
}

#[tokio::test]
async fn unexpected_adapter_failure_is_stable_and_secret_free() {
    let fixture = Fixture::new(SHA_A, SHA_A).open_error(PortErrorKind::Unexpected);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_secret_free(&result);
}

#[tokio::test]
async fn apply_target_must_equal_fresh_github_tip() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .apply_enabled();
    let result = fixture.apply(SHA_A, APPROVAL).await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.github_target, Some(oid(SHA_A)));
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn apply_requires_an_approval_event() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .apply_enabled();
    let mut request = ReconcileRequest::observe();
    request.mode = buzz_git_relay::ReconcileMode::Apply;
    request.expected_target = Some(oid(SHA_B));
    let result = fixture
        .reconciler
        .reconcile(&fixture.enrollment, request)
        .await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
}

#[tokio::test]
async fn already_equal_apply_still_requires_exact_approval() {
    let fixture = Fixture::new(SHA_A, SHA_A).apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
}

#[tokio::test]
async fn concurrent_relay_only_change_is_reclassified_without_push() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .ancestor(SHA_B, SHA_D)
        .refresh_buzz(SHA_D)
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::GithubBehind,
        MutationEvidence::None,
        1,
    );
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn concurrent_divergent_change_is_reclassified_without_push() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_buzz(SHA_D)
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(&result, Classification::Diverged, MutationEvidence::None, 1);
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn github_change_before_push_freezes_old_approval() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_both(vec![Ok(tips(SHA_D, SHA_A))])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.github_target, Some(oid(SHA_B)));
}

#[tokio::test]
async fn buzz_change_during_final_pre_push_check_freezes_without_push() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_both(vec![Ok(tips(SHA_B, SHA_D))])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.buzz_after, Some(oid(SHA_D)));
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn rejected_push_with_unchanged_buzz_records_no_mutation() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .push_error(PortErrorKind::Config, false)
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.push_attempted, Some(true));
}

#[tokio::test]
async fn ambiguous_push_is_resolved_by_exact_reread() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .push_error(PortErrorKind::Transient, true)
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::InSync,
        MutationEvidence::FastForward,
        0,
    );
}

#[tokio::test]
async fn attempted_push_with_unreadable_outcome_is_unknown() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .push_error(PortErrorKind::Transient, false)
        .refresh_both(vec![
            Ok(tips(SHA_B, SHA_A)),
            Err(port(PortErrorKind::Transient)),
        ])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::Unknown,
        1,
    );
    assert_eq!(result.buzz_after, None);
    assert!(result.retryable);
}

#[tokio::test]
async fn successful_push_with_failed_reread_is_unknown() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_both(vec![
            Ok(tips(SHA_B, SHA_A)),
            Err(port(PortErrorKind::Verify)),
        ])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::Unknown,
        1,
    );
    assert_eq!(result.buzz_after, None);
}

#[tokio::test]
async fn unexpected_failure_after_push_is_unknown_and_secret_free() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .push_error(PortErrorKind::Unexpected, false)
        .refresh_both(vec![
            Ok(tips(SHA_B, SHA_A)),
            Err(port(PortErrorKind::Unexpected)),
        ])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::Unknown,
        1,
    );
    assert_secret_free(&result);
}

#[tokio::test]
async fn successful_push_with_unequal_refs_is_verify_error() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_both(vec![Ok(tips(SHA_B, SHA_A)), Ok(tips(SHA_B, SHA_D))])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::Unknown,
        1,
    );
}

#[tokio::test]
async fn github_advance_after_delivery_records_confirmed_fast_forward() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_both(vec![Ok(tips(SHA_B, SHA_A)), Ok(tips(SHA_D, SHA_B))])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::FastForward,
        1,
    );
}

#[tokio::test]
async fn github_advance_with_unchanged_buzz_does_not_claim_delivery() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_both(vec![Ok(tips(SHA_B, SHA_A)), Ok(tips(SHA_D, SHA_A))])
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
}

#[tokio::test]
async fn duplicate_verified_apply_returns_prior_without_duplicate_evidence() {
    let fixture = Fixture::new(SHA_A, SHA_A).apply_enabled();
    let first = fixture.apply(SHA_A, APPROVAL).await;
    let second = fixture.apply(SHA_A, APPROVAL).await;
    assert_eq!(second.run_id, first.run_id);
    assert_eq!(fixture.evidence.records().len(), 1);
}

#[tokio::test]
async fn cleanup_failure_during_replay_describes_current_no_push_invocation() {
    let fixture = Fixture::new(SHA_A, SHA_A).apply_enabled();
    let first = fixture.apply(SHA_A, APPROVAL).await;
    fixture.locks.state.lock().unwrap().release_error = true;

    let replay_cleanup = fixture.apply(SHA_A, APPROVAL).await;

    assert_ne!(replay_cleanup.run_id, first.run_id);
    assert_result(
        &replay_cleanup,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(replay_cleanup.push_attempted, Some(false));
}

#[tokio::test]
async fn observe_record_cannot_satisfy_later_apply() {
    let fixture = Fixture::new(SHA_A, SHA_A).apply_enabled();
    let observed = fixture.observe().await;
    let applied = fixture.apply(SHA_A, APPROVAL).await;
    assert_ne!(applied.run_id, observed.run_id);
    assert_eq!(fixture.evidence.records().len(), 2);
}

#[tokio::test]
async fn new_approval_gets_a_new_apply_record() {
    let fixture = Fixture::new(SHA_A, SHA_A).apply_enabled();
    let first = fixture.apply(SHA_A, APPROVAL).await;
    let second = fixture.apply(SHA_A, OTHER_APPROVAL).await;
    assert_ne!(first.run_id, second.run_id);
    assert_eq!(fixture.evidence.records().len(), 2);
}

#[tokio::test]
async fn repointed_repository_identity_cannot_reuse_old_evidence() {
    let mut fixture = Fixture::new(SHA_A, SHA_A).apply_enabled();
    let first = fixture.apply(SHA_A, APPROVAL).await;
    fixture.enrollment.github_repository_id = GithubRepositoryId::new("67890").unwrap();
    let second = fixture.apply(SHA_A, APPROVAL).await;
    assert_ne!(first.run_id, second.run_id);
    assert_eq!(fixture.evidence.records().len(), 2);
}

#[tokio::test]
async fn busy_lock_is_retryable_and_does_not_open_repository() {
    let fixture = Fixture::new(SHA_A, SHA_A).lock_error(PortErrorKind::Transient);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::TransientError,
        MutationEvidence::None,
        1,
    );
    assert!(result.retryable);
    assert_eq!(fixture.state().open_calls, 0);
}

#[tokio::test]
async fn session_close_failure_cannot_mask_machine_result() {
    let fixture = Fixture::new(SHA_A, SHA_A).close_error();
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.next_action, NextAction::InspectCleanupFailure);
    assert_secret_free(&result);
}

#[tokio::test]
async fn close_failure_after_verified_push_preserves_fast_forward_evidence() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .close_error()
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::FastForward,
        1,
    );
    assert_eq!(result.buzz_after, Some(oid(SHA_B)));
}

#[tokio::test]
async fn release_failure_cannot_mask_machine_result() {
    let fixture = Fixture::new(SHA_A, SHA_A).release_error();
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_secret_free(&result);
}

#[tokio::test]
async fn release_failure_after_verified_push_preserves_fast_forward_evidence() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .release_error()
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::FastForward,
        1,
    );
    assert_eq!(result.buzz_after, Some(oid(SHA_B)));
}

#[tokio::test]
async fn audit_failure_returns_stable_secret_free_result() {
    let fixture = Fixture::new(SHA_A, SHA_A).append_error();
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.next_action, NextAction::RepairAuditStore);
    assert_secret_free(&result);
}

#[tokio::test]
async fn audit_failure_after_verified_push_preserves_fast_forward_evidence() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .append_error()
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::FastForward,
        1,
    );
    assert_eq!(result.buzz_after, Some(oid(SHA_B)));
}

#[tokio::test]
async fn missing_github_main_fails_closed_as_configuration_error() {
    let fixture = Fixture::new(SHA_A, SHA_A).tips_error(PortErrorKind::Config);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn missing_buzz_main_fails_closed_as_configuration_error() {
    let fixture = Fixture::new(SHA_A, SHA_A).tips_error(PortErrorKind::Config);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::ConfigError,
        MutationEvidence::None,
        1,
    );
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn evidence_lookup_failure_emits_stable_machine_result() {
    let fixture = Fixture::new(SHA_A, SHA_A)
        .apply_enabled()
        .find_error(PortErrorKind::Unexpected);
    let result = fixture.apply(SHA_A, APPROVAL).await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_secret_free(&result);
}

#[tokio::test]
async fn unexpected_ancestry_failure_emits_stable_machine_result() {
    let fixture = Fixture::new(SHA_B, SHA_A).ancestor_error(PortErrorKind::Unexpected);
    let result = fixture.observe().await;
    assert_result(
        &result,
        Classification::VerifyError,
        MutationEvidence::None,
        1,
    );
    assert_secret_free(&result);
}

#[tokio::test]
async fn pre_push_refresh_failure_never_attempts_a_push() {
    let fixture = Fixture::new(SHA_B, SHA_A)
        .ancestor(SHA_A, SHA_B)
        .refresh_buzz_error(PortErrorKind::Transient)
        .apply_enabled();
    let result = fixture.apply(SHA_B, APPROVAL).await;
    assert_result(
        &result,
        Classification::TransientError,
        MutationEvidence::None,
        1,
    );
    assert_eq!(result.push_attempted, Some(false));
    assert!(fixture.state().pushes.is_empty());
}

#[tokio::test]
async fn one_repository_failure_does_not_poison_a_later_invocation() {
    let failing = Fixture::new(SHA_A, SHA_A).open_error(PortErrorKind::Auth);
    let healthy = Fixture::new(SHA_A, SHA_A);
    let failed = failing.observe().await;
    let succeeded = healthy.observe().await;
    assert_eq!(failed.classification, Classification::AuthError);
    assert_eq!(succeeded.classification, Classification::InSync);
}

fn assert_result(
    result: &ReconcileResult,
    classification: Classification,
    mutation: MutationEvidence,
    exit_code: u8,
) {
    assert_eq!(result.classification, classification);
    assert_eq!(result.mutation, mutation);
    assert_eq!(result.exit_code, exit_code);
}

fn assert_secret_free(result: &ReconcileResult) {
    let json = serde_json::to_string(result).unwrap();
    assert!(!json.contains("secret"));
    assert!(!json.contains("credential"));
}

fn oid(value: &str) -> CommitOid {
    CommitOid::new(value).unwrap()
}

fn tips(github: &str, buzz: &str) -> Tips {
    Tips {
        github: oid(github),
        buzz: oid(buzz),
    }
}

fn port(kind: PortErrorKind) -> PortError {
    PortError::new(kind)
}

struct Fixture {
    enrollment: Enrollment,
    reconciler: Reconciler,
    repository: Arc<FakeRepository>,
    evidence: Arc<MemoryEvidence>,
    locks: Arc<FakeLocks>,
}

impl Fixture {
    fn new(github: &str, buzz: &str) -> Self {
        let repository = Arc::new(FakeRepository::new(github, buzz));
        let evidence = Arc::new(MemoryEvidence::default());
        let locks = Arc::new(FakeLocks::default());
        let reconciler = Reconciler::new(
            repository.clone(),
            locks.clone(),
            evidence.clone(),
            Arc::new(SequentialRunIds::default()),
        );
        Self {
            enrollment: Enrollment::enabled(
                EnrollmentId::new("buzz-workspace").unwrap(),
                GithubRepositoryId::new("12345").unwrap(),
                ManagedRef::main(),
            ),
            reconciler,
            repository,
            evidence,
            locks,
        }
    }

    fn state(&self) -> FakeState {
        self.repository.state.lock().unwrap().clone()
    }

    fn ancestor(self, ancestor: &str, descendant: &str) -> Self {
        self.repository
            .state
            .lock()
            .unwrap()
            .ancestors
            .insert((oid(ancestor), oid(descendant)));
        self
    }

    fn apply_enabled(mut self) -> Self {
        self.enrollment = self.enrollment.clone().with_owner_approved_apply();
        self
    }

    fn open_error(self, kind: PortErrorKind) -> Self {
        self.repository.state.lock().unwrap().open_error = Some(port(kind));
        self
    }

    fn tips_error(self, kind: PortErrorKind) -> Self {
        self.repository.state.lock().unwrap().tips_error = Some(port(kind));
        self
    }

    fn ancestor_error(self, kind: PortErrorKind) -> Self {
        self.repository.state.lock().unwrap().ancestor_error = Some(port(kind));
        self
    }

    fn refresh_buzz(self, value: &str) -> Self {
        self.repository.state.lock().unwrap().refresh_buzz = Some(Ok(oid(value)));
        self
    }

    fn refresh_buzz_error(self, kind: PortErrorKind) -> Self {
        self.repository.state.lock().unwrap().refresh_buzz = Some(Err(port(kind)));
        self
    }

    fn refresh_both(self, values: Vec<Result<Tips, PortError>>) -> Self {
        self.repository.state.lock().unwrap().refresh_both = values.into();
        self
    }

    fn push_error(self, kind: PortErrorKind, apply_target_before_error: bool) -> Self {
        let mut state = self.repository.state.lock().unwrap();
        state.push_error = Some(port(kind));
        state.apply_target_before_push_error = apply_target_before_error;
        drop(state);
        self
    }

    fn close_error(self) -> Self {
        self.repository.state.lock().unwrap().close_error = true;
        self
    }

    fn lock_error(self, kind: PortErrorKind) -> Self {
        self.locks.state.lock().unwrap().acquire_error = Some(port(kind));
        self
    }

    fn release_error(self) -> Self {
        self.locks.state.lock().unwrap().release_error = true;
        self
    }

    fn append_error(self) -> Self {
        self.evidence.state.lock().unwrap().append_error = true;
        self
    }

    fn find_error(self, kind: PortErrorKind) -> Self {
        self.evidence.state.lock().unwrap().find_error = Some(port(kind));
        self
    }

    async fn observe(&self) -> ReconcileResult {
        self.reconciler
            .reconcile(&self.enrollment, ReconcileRequest::observe())
            .await
    }

    async fn apply(&self, target: &str, approval: &str) -> ReconcileResult {
        self.reconciler
            .reconcile(
                &self.enrollment,
                ReconcileRequest::apply(oid(target), ApprovalEventId::new(approval).unwrap()),
            )
            .await
    }
}

#[derive(Clone)]
struct FakeState {
    tips: Tips,
    ancestors: HashSet<(CommitOid, CommitOid)>,
    refresh_buzz: Option<Result<CommitOid, PortError>>,
    refresh_both: VecDeque<Result<Tips, PortError>>,
    open_error: Option<PortError>,
    tips_error: Option<PortError>,
    ancestor_error: Option<PortError>,
    push_error: Option<PortError>,
    apply_target_before_push_error: bool,
    close_error: bool,
    pushes: Vec<(String, String)>,
    open_calls: usize,
}

struct FakeRepository {
    state: Arc<Mutex<FakeState>>,
}

impl FakeRepository {
    fn new(github: &str, buzz: &str) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                tips: tips(github, buzz),
                ancestors: HashSet::new(),
                refresh_buzz: None,
                refresh_both: VecDeque::new(),
                open_error: None,
                tips_error: None,
                ancestor_error: None,
                push_error: None,
                apply_target_before_push_error: false,
                close_error: false,
                pushes: Vec::new(),
                open_calls: 0,
            })),
        }
    }
}

#[async_trait::async_trait]
impl RepositoryPort for FakeRepository {
    async fn open(
        &self,
        _enrollment: &Enrollment,
    ) -> Result<Box<dyn RepositorySession>, PortError> {
        let mut state = self.state.lock().unwrap();
        state.open_calls += 1;
        if let Some(error) = state.open_error.clone() {
            return Err(error);
        }
        Ok(Box::new(FakeSession {
            state: self.state.clone(),
        }))
    }
}

struct FakeSession {
    state: Arc<Mutex<FakeState>>,
}

#[async_trait::async_trait]
impl RepositorySession for FakeSession {
    async fn tips(&mut self) -> Result<Tips, PortError> {
        let state = self.state.lock().unwrap();
        if let Some(error) = state.tips_error.clone() {
            return Err(error);
        }
        Ok(state.tips.clone())
    }

    async fn is_ancestor(
        &mut self,
        ancestor: &CommitOid,
        descendant: &CommitOid,
    ) -> Result<bool, PortError> {
        let state = self.state.lock().unwrap();
        if let Some(error) = state.ancestor_error.clone() {
            return Err(error);
        }
        Ok(state
            .ancestors
            .contains(&(ancestor.clone(), descendant.clone())))
    }

    async fn refresh_buzz(&mut self) -> Result<CommitOid, PortError> {
        let state = self.state.lock().unwrap();
        state
            .refresh_buzz
            .clone()
            .unwrap_or_else(|| Ok(state.tips.buzz.clone()))
    }

    async fn refresh_both(&mut self) -> Result<Tips, PortError> {
        let mut state = self.state.lock().unwrap();
        state
            .refresh_both
            .pop_front()
            .unwrap_or_else(|| Ok(state.tips.clone()))
    }

    async fn push_exact(
        &mut self,
        target: &CommitOid,
        managed_ref: &ManagedRef,
    ) -> Result<(), PortError> {
        let mut state = self.state.lock().unwrap();
        state
            .pushes
            .push((target.as_str().to_owned(), managed_ref.as_str().to_owned()));
        if state.push_error.is_none() || state.apply_target_before_push_error {
            state.tips.buzz = target.clone();
        }
        match state.push_error.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn close(self: Box<Self>) -> Result<(), PortError> {
        if self.state.lock().unwrap().close_error {
            Err(port(PortErrorKind::Unexpected))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct EvidenceState {
    records: Vec<ReconcileResult>,
    find_error: Option<PortError>,
    append_error: bool,
}

#[derive(Default)]
struct MemoryEvidence {
    state: Mutex<EvidenceState>,
}

impl MemoryEvidence {
    fn records(&self) -> Vec<ReconcileResult> {
        self.state.lock().unwrap().records.clone()
    }
}

#[async_trait::async_trait]
impl EvidenceStore for MemoryEvidence {
    async fn find_verified(&self, key: &ReplayKey) -> Result<Option<ReconcileResult>, PortError> {
        let state = self.state.lock().unwrap();
        if let Some(error) = state.find_error.clone() {
            return Err(error);
        }
        Ok(state
            .records
            .iter()
            .find(|record| {
                record.enrollment_id == key.enrollment_id
                    && record.github_repository_id == key.github_repository_id
                    && record.managed_ref == key.managed_ref
                    && record.github_target.as_ref() == Some(&key.target)
                    && record.buzz_after.as_ref() == Some(&key.target)
                    && record.approval_event.as_ref() == Some(&key.approval_event)
                    && record.classification == Classification::InSync
                    && record.mode == buzz_git_relay::ReconcileMode::Apply
                    && record.exit_code == 0
            })
            .cloned())
    }

    async fn append(&self, result: &ReconcileResult) -> Result<(), PortError> {
        let mut state = self.state.lock().unwrap();
        if state.append_error {
            return Err(port(PortErrorKind::Unexpected));
        }
        state.records.push(result.clone());
        Ok(())
    }
}

#[derive(Default)]
struct LockState {
    acquire_error: Option<PortError>,
    release_error: bool,
}

#[derive(Default)]
struct FakeLocks {
    state: Arc<Mutex<LockState>>,
}

#[async_trait::async_trait]
impl LockManager for FakeLocks {
    async fn acquire(&self, _key: &str) -> Result<Box<dyn LockLease>, PortError> {
        if let Some(error) = self.state.lock().unwrap().acquire_error.clone() {
            return Err(error);
        }
        Ok(Box::new(FakeLease {
            state: self.state.clone(),
        }))
    }
}

struct FakeLease {
    state: Arc<Mutex<LockState>>,
}

#[async_trait::async_trait]
impl LockLease for FakeLease {
    async fn release(self: Box<Self>) -> Result<(), PortError> {
        if self.state.lock().unwrap().release_error {
            Err(port(PortErrorKind::Unexpected))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct SequentialRunIds {
    next: Mutex<u64>,
}

impl RunIdGenerator for SequentialRunIds {
    fn next(&self) -> RunId {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        RunId::new(format!("run-{next}")).unwrap()
    }
}
