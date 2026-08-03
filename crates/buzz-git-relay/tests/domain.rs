use buzz_git_relay::{
    ApprovalEventId, CommitOid, Enrollment, EnrollmentId, GithubRepositoryId, ManagedRef,
    ReconcileRequest, RolloutPhase, RunId,
};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const APPROVAL: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[test]
fn rejects_empty_enrollment_identity() {
    assert!(EnrollmentId::new(" ").is_err());
}

#[test]
fn rejects_empty_repository_identity() {
    assert!(GithubRepositoryId::new("").is_err());
}

#[test]
fn rejects_uppercase_or_short_commit_ids() {
    assert!(CommitOid::new("A".repeat(40)).is_err());
    assert!(CommitOid::new("a".repeat(39)).is_err());
}

#[test]
fn rejects_malformed_approval_event_ids() {
    assert!(ApprovalEventId::new("c".repeat(63)).is_err());
    assert!(ApprovalEventId::new("z".repeat(64)).is_err());
}

#[test]
fn managed_ref_is_an_exact_allowlist() {
    assert_eq!(ManagedRef::main().as_str(), "refs/heads/main");
    assert!(ManagedRef::new("refs/heads/other").is_err());
    assert!(ManagedRef::new("refs/heads/*").is_err());
}

#[test]
fn deserialization_preserves_validated_domain_invariants() {
    assert!(serde_json::from_str::<EnrollmentId>(r#""bad id""#).is_err());
    assert!(serde_json::from_str::<GithubRepositoryId>(r#""""#).is_err());
    assert!(serde_json::from_str::<RunId>(r#""run id""#).is_err());
    assert!(serde_json::from_str::<CommitOid>(r#""abc""#).is_err());
    assert!(serde_json::from_str::<ApprovalEventId>(r#""not-an-event""#).is_err());
    assert!(serde_json::from_str::<ManagedRef>(r#""refs/heads/other""#).is_err());

    let managed_ref = serde_json::from_str::<ManagedRef>(r#""refs/heads/main""#).unwrap();
    assert_eq!(managed_ref, ManagedRef::main());
}

#[test]
fn enrollment_defaults_to_observe_only_and_requires_approval() {
    let enrollment = enrollment();
    assert_eq!(enrollment.rollout_phase, RolloutPhase::ObserveOnly);
    assert!(!enrollment.apply_enabled);
    assert!(enrollment.approval_required);
}

#[test]
fn apply_request_binds_exact_target_and_approval() {
    let target = CommitOid::new(SHA).unwrap();
    let approval = ApprovalEventId::new(APPROVAL).unwrap();
    let request = ReconcileRequest::apply(target.clone(), approval.clone());
    assert_eq!(request.expected_target, Some(target));
    assert_eq!(request.approval_event, Some(approval));
}

#[test]
fn serialized_enrollment_contains_no_credentials_or_clone_urls() {
    let json = serde_json::to_string(&enrollment()).unwrap();
    assert!(!json.contains("credential"));
    assert!(!json.contains("token"));
    assert!(!json.contains("clone_url"));
    assert!(json.contains("github_repository_id"));
}

fn enrollment() -> Enrollment {
    Enrollment::enabled(
        EnrollmentId::new("example").unwrap(),
        GithubRepositoryId::new("12345").unwrap(),
        ManagedRef::main(),
    )
}
