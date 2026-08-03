use crate::ports::{
    EvidenceStore, LockLease, LockManager, PortError, PortErrorKind, RepositoryPort,
    RepositorySession, RunIdGenerator,
};
use crate::{
    Classification, CommitOid, Enrollment, ExitOutcome, MutationEvidence, NextAction,
    ReconcileMode, ReconcileRequest, ReconcileResult, ReplayKey, RolloutPhase, RunId, Tips,
};
use std::sync::Arc;

/// Deterministic reconciliation state machine.
pub struct Reconciler {
    repositories: Arc<dyn RepositoryPort>,
    locks: Arc<dyn LockManager>,
    evidence: Arc<dyn EvidenceStore>,
    run_ids: Arc<dyn RunIdGenerator>,
}

impl Reconciler {
    /// Constructs a reconciler over host-provided boundary adapters.
    pub fn new(
        repositories: Arc<dyn RepositoryPort>,
        locks: Arc<dyn LockManager>,
        evidence: Arc<dyn EvidenceStore>,
        run_ids: Arc<dyn RunIdGenerator>,
    ) -> Self {
        Self {
            repositories,
            locks,
            evidence,
            run_ids,
        }
    }

    /// Classifies one enrolled repository and, when explicitly authorized,
    /// performs only a proven exact fast-forward.
    pub async fn reconcile(
        &self,
        enrollment: &Enrollment,
        request: ReconcileRequest,
    ) -> ReconcileResult {
        let run_id = self.run_ids.next();
        if !enrollment.enabled {
            return self
                .record(base_result(
                    run_id,
                    enrollment,
                    &request,
                    Classification::Unmanaged,
                    NextAction::None,
                    "Git-relay is disabled for this repository.",
                    false,
                ))
                .await;
        }
        if !enrollment.policy_is_valid() {
            return self
                .record(base_result(
                    run_id,
                    enrollment,
                    &request,
                    Classification::ConfigError,
                    NextAction::FixConfiguration,
                    "Enrollment policy validation failed; Git-relay made no changes.",
                    false,
                ))
                .await;
        }

        let lock_key = format!("{}:{}", enrollment.id, enrollment.managed_ref.as_str());
        let lease = match self.locks.acquire(&lock_key).await {
            Ok(lease) => lease,
            Err(error) => {
                let (classification, retryable, action, summary) = match error.kind() {
                    PortErrorKind::Transient => (
                        Classification::TransientError,
                        true,
                        NextAction::RetryAfterActiveRun,
                        "Another Git-relay run holds this repository/ref lock.",
                    ),
                    _ => (
                        Classification::VerifyError,
                        false,
                        NextAction::InspectUnexpectedFailure,
                        "An unexpected lock boundary failed; inspect trusted host logs.",
                    ),
                };
                let mut result = base_result(
                    run_id,
                    enrollment,
                    &request,
                    classification,
                    action,
                    summary,
                    false,
                );
                result.retryable = retryable;
                return self.record(result).await;
            }
        };

        let session = self.repositories.open(enrollment).await;
        let mut session = match session {
            Ok(session) => session,
            Err(error) => {
                let result =
                    boundary_result(run_id, enrollment, &request, &error, false, None, None);
                return self.finalize(result, None, Some(lease), false).await;
            }
        };

        let tips = match session.tips().await {
            Ok(tips) => tips,
            Err(error) => {
                let result =
                    boundary_result(run_id, enrollment, &request, &error, false, None, None);
                return self
                    .finalize(result, Some(session), Some(lease), false)
                    .await;
            }
        };

        let github_before = Some(tips.github.clone());
        let buzz_before = Some(tips.buzz.clone());
        let mut result = classified_result(run_id, enrollment, &request, &tips);

        if request.mode == ReconcileMode::Apply
            && !apply_authorized(enrollment, &request, &tips.github)
        {
            result.classification = Classification::ConfigError;
            result.next_action = NextAction::ProvideExactTargetAndApproval;
            result.summary = "Apply requires Phase 2 enablement, the current GitHub target, and a valid approval event ID.".to_owned();
            result.outcome = ExitOutcome::Failure;
            result.exit_code = 1;
            return self
                .finalize(result, Some(session), Some(lease), false)
                .await;
        }

        if request.mode == ReconcileMode::Apply && tips.github == tips.buzz {
            if let Some(approval_event) = request.approval_event.clone() {
                let key = ReplayKey {
                    enrollment_id: enrollment.id.clone(),
                    github_repository_id: enrollment.github_repository_id.clone(),
                    managed_ref: enrollment.managed_ref.clone(),
                    target: tips.github.clone(),
                    approval_event,
                };
                // Replay is valid only for the complete immutable delivery key.
                match self.evidence.find_verified(&key).await {
                    Ok(Some(prior)) => {
                        return self
                            .finalize_replay(
                                prior,
                                result.run_id.clone(),
                                Some(session),
                                Some(lease),
                            )
                            .await;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let result = boundary_result(
                            result.run_id.clone(),
                            enrollment,
                            &request,
                            &error,
                            false,
                            github_before,
                            buzz_before,
                        );
                        return self
                            .finalize(result, Some(session), Some(lease), false)
                            .await;
                    }
                }
            }
        }

        let classification = match self.classify(&mut *session, &tips).await {
            Ok(classification) => classification,
            Err(error) => {
                let result = boundary_result(
                    result.run_id.clone(),
                    enrollment,
                    &request,
                    &error,
                    false,
                    github_before,
                    buzz_before,
                );
                return self
                    .finalize(result, Some(session), Some(lease), false)
                    .await;
            }
        };
        apply_classification(&mut result, classification, request.mode);

        if request.mode != ReconcileMode::Apply || classification != Classification::RelayBehind {
            return self
                .finalize(result, Some(session), Some(lease), false)
                .await;
        }

        let current_buzz = match session.refresh_buzz().await {
            Ok(tip) => tip,
            Err(error) => {
                let result = boundary_result(
                    result.run_id.clone(),
                    enrollment,
                    &request,
                    &error,
                    false,
                    github_before,
                    buzz_before,
                );
                return self
                    .finalize(result, Some(session), Some(lease), false)
                    .await;
            }
        };
        if current_buzz == tips.github {
            result.classification = Classification::InSync;
            result.buzz_after = Some(current_buzz);
            succeed(
                &mut result,
                "Buzz reached the target before Git-relay pushed.",
            );
            return self
                .finalize(result, Some(session), Some(lease), false)
                .await;
        }
        let still_safe = match session.is_ancestor(&current_buzz, &tips.github).await {
            Ok(value) => value,
            Err(error) => {
                let result = boundary_result(
                    result.run_id.clone(),
                    enrollment,
                    &request,
                    &error,
                    false,
                    github_before,
                    buzz_before,
                );
                return self
                    .finalize(result, Some(session), Some(lease), false)
                    .await;
            }
        };
        if !still_safe {
            let github_behind = match session.is_ancestor(&tips.github, &current_buzz).await {
                Ok(value) => value,
                Err(error) => {
                    let result = boundary_result(
                        result.run_id.clone(),
                        enrollment,
                        &request,
                        &error,
                        false,
                        github_before,
                        buzz_before,
                    );
                    return self
                        .finalize(result, Some(session), Some(lease), false)
                        .await;
                }
            };
            result.classification = if github_behind {
                Classification::GithubBehind
            } else {
                Classification::Diverged
            };
            result.buzz_after = Some(current_buzz);
            if github_behind {
                fail(
                    &mut result,
                    NextAction::InspectRelayOnlyCommits,
                    "Buzz advanced after classification and now contains relay-only commits.",
                );
            } else {
                fail(
                    &mut result,
                    NextAction::ReviewDivergentHistories,
                    "Buzz changed after classification and the histories now diverge.",
                );
            }
            return self
                .finalize(result, Some(session), Some(lease), false)
                .await;
        }

        // Keep this as the final remote read before the exact-ref push race fence.
        let before_push = match session.refresh_both().await {
            Ok(tips) => tips,
            Err(error) => {
                let result = boundary_result(
                    result.run_id.clone(),
                    enrollment,
                    &request,
                    &error,
                    false,
                    github_before,
                    buzz_before,
                );
                return self
                    .finalize(result, Some(session), Some(lease), false)
                    .await;
            }
        };
        if before_push.github != tips.github {
            result.classification = Classification::ConfigError;
            result.buzz_after = Some(before_push.buzz);
            fail(
                &mut result,
                NextAction::ApproveNewGithubTarget,
                "GitHub advanced after approval; Git-relay froze the old target.",
            );
            return self
                .finalize(result, Some(session), Some(lease), false)
                .await;
        }
        if before_push.buzz != current_buzz {
            result.buzz_after = Some(before_push.buzz.clone());
            if before_push.buzz == tips.github {
                result.classification = Classification::InSync;
                succeed(
                    &mut result,
                    "Buzz reached the target before Git-relay pushed.",
                );
            } else {
                result.classification = Classification::VerifyError;
                fail(
                    &mut result,
                    NextAction::RetryAfterConcurrentChange,
                    "Buzz changed during the final pre-push check; Git-relay froze without pushing.",
                );
            }
            return self
                .finalize(result, Some(session), Some(lease), false)
                .await;
        }

        result.push_attempted = Some(true);
        let push_error = session
            .push_exact(&tips.github, &enrollment.managed_ref)
            .await
            .err();
        // A push response is not mutation evidence; only this immediate reread is.
        let after = session.refresh_both().await;
        match after {
            Ok(after) => {
                result.buzz_after = Some(after.buzz.clone());
                result.mutation = if after.buzz == tips.github {
                    MutationEvidence::FastForward
                } else if after.buzz == current_buzz {
                    MutationEvidence::None
                } else {
                    MutationEvidence::Unknown
                };
                if after.github == tips.github && after.buzz == tips.github {
                    result.classification = Classification::InSync;
                    let summary = if push_error.is_some() {
                        "The push response was ambiguous, but an immediate reread verified exact target equality."
                    } else {
                        "Buzz was fast-forwarded to the exact GitHub target and verified."
                    };
                    succeed(&mut result, summary);
                } else if let Some(error) = push_error {
                    if after.buzz == current_buzz {
                        let (classification, retryable, action) = mapped_error(&error);
                        result.classification = classification;
                        result.retryable = retryable;
                        fail(&mut result, action, "The push failed and an immediate reread confirmed that Buzz did not change.");
                    } else {
                        result.classification = Classification::VerifyError;
                        fail(&mut result, NextAction::InspectAmbiguousPush, "Buzz changed to an unexpected SHA during an attempted push; mutation attribution is unknown.");
                    }
                } else {
                    result.classification = Classification::VerifyError;
                    let (action, summary) = if after.github != tips.github {
                        (
                            NextAction::ApproveNewGithubTarget,
                            if result.mutation == MutationEvidence::FastForward {
                                "Buzz reached the approved target, but GitHub advanced before post-push verification."
                            } else {
                                "GitHub advanced before delivery could be verified."
                            },
                        )
                    } else {
                        (
                            NextAction::InspectPostPushRefs,
                            "The push completed but post-push refs did not match the target.",
                        )
                    };
                    fail(&mut result, action, summary);
                }
            }
            Err(error) => {
                result.classification = Classification::VerifyError;
                result.buzz_after = None;
                result.mutation = MutationEvidence::Unknown;
                result.retryable = push_error
                    .as_ref()
                    .is_some_and(|value| value.kind() == PortErrorKind::Transient)
                    || error.kind() == PortErrorKind::Transient;
                fail(&mut result, NextAction::InspectAmbiguousPush, "A push was attempted, but its outcome could not be reread; mutation is unknown.");
            }
        }
        self.finalize(result, Some(session), Some(lease), true)
            .await
    }

    async fn classify(
        &self,
        session: &mut dyn RepositorySession,
        tips: &Tips,
    ) -> Result<Classification, PortError> {
        if tips.github == tips.buzz {
            return Ok(Classification::InSync);
        }
        if session.is_ancestor(&tips.buzz, &tips.github).await? {
            return Ok(Classification::RelayBehind);
        }
        if session.is_ancestor(&tips.github, &tips.buzz).await? {
            return Ok(Classification::GithubBehind);
        }
        Ok(Classification::Diverged)
    }

    async fn finalize(
        &self,
        mut result: ReconcileResult,
        session: Option<Box<dyn RepositorySession>>,
        lease: Option<Box<dyn LockLease>>,
        push_attempted: bool,
    ) -> ReconcileResult {
        // Record after cleanup so cleanup failure and proved mutation remain one result.
        if cleanup_failed(session, lease).await {
            result.classification = Classification::VerifyError;
            result.push_attempted = Some(push_attempted);
            fail(
                &mut result,
                NextAction::InspectCleanupFailure,
                "Repository session cleanup failed; inspect trusted host logs.",
            );
            return self.record(result).await;
        }
        self.record(result).await
    }

    async fn finalize_replay(
        &self,
        prior: ReconcileResult,
        current_run_id: RunId,
        session: Option<Box<dyn RepositorySession>>,
        lease: Option<Box<dyn LockLease>>,
    ) -> ReconcileResult {
        if !cleanup_failed(session, lease).await {
            return prior;
        }

        let mut result = prior;
        result.run_id = current_run_id;
        result.classification = Classification::VerifyError;
        result.push_attempted = Some(false);
        result.mutation = MutationEvidence::None;
        fail(
            &mut result,
            NextAction::InspectCleanupFailure,
            "Repository session cleanup failed; inspect trusted host logs.",
        );
        self.record(result).await
    }

    async fn record(&self, mut result: ReconcileResult) -> ReconcileResult {
        if self.evidence.append(&result).await.is_err() {
            result.classification = Classification::VerifyError;
            fail(
                &mut result,
                NextAction::RepairAuditStore,
                "Audit evidence could not be persisted; inspect trusted host logs.",
            );
        }
        result
    }
}

async fn cleanup_failed(
    session: Option<Box<dyn RepositorySession>>,
    lease: Option<Box<dyn LockLease>>,
) -> bool {
    let session_failed = match session {
        Some(session) => session.close().await.is_err(),
        None => false,
    };
    let lease_failed = match lease {
        Some(lease) => lease.release().await.is_err(),
        None => false,
    };
    session_failed || lease_failed
}

fn apply_authorized(
    enrollment: &Enrollment,
    request: &ReconcileRequest,
    current: &CommitOid,
) -> bool {
    enrollment.rollout_phase == RolloutPhase::OwnerApprovedApply
        && enrollment.apply_enabled
        && enrollment.approval_required
        && request.expected_target.as_ref() == Some(current)
        && request.approval_event.is_some()
}

fn classified_result(
    run_id: RunId,
    enrollment: &Enrollment,
    request: &ReconcileRequest,
    tips: &Tips,
) -> ReconcileResult {
    let mut result = base_result(
        run_id,
        enrollment,
        request,
        Classification::InSync,
        NextAction::None,
        "GitHub and Buzz refs are in sync.",
        false,
    );
    result.github_before = Some(tips.github.clone());
    result.buzz_before = Some(tips.buzz.clone());
    result.buzz_after = Some(tips.buzz.clone());
    result
}

fn apply_classification(
    result: &mut ReconcileResult,
    classification: Classification,
    mode: ReconcileMode,
) {
    result.classification = classification;
    match classification {
        Classification::InSync => succeed(result, "GitHub and Buzz refs are in sync."),
        Classification::RelayBehind => fail(
            result,
            NextAction::ApproveExactFastForward,
            if mode == ReconcileMode::Observe {
                "Buzz is behind GitHub; observe mode made no changes."
            } else {
                "Buzz is behind GitHub; an exact fast-forward is eligible."
            },
        ),
        Classification::GithubBehind => fail(
            result,
            NextAction::InspectRelayOnlyCommits,
            "Buzz contains commits not present on GitHub; Git-relay froze.",
        ),
        Classification::Diverged => fail(
            result,
            NextAction::ReviewDivergentHistories,
            "GitHub and Buzz histories diverged; Git-relay froze.",
        ),
        _ => {}
    }
}

fn boundary_result(
    run_id: RunId,
    enrollment: &Enrollment,
    request: &ReconcileRequest,
    error: &PortError,
    push_attempted: bool,
    github_before: Option<CommitOid>,
    buzz_before: Option<CommitOid>,
) -> ReconcileResult {
    let (classification, retryable, action) = mapped_error(error);
    let mut result = base_result(
        run_id,
        enrollment,
        request,
        if push_attempted {
            Classification::VerifyError
        } else {
            classification
        },
        action,
        "A reconciliation boundary failed; inspect trusted host logs.",
        false,
    );
    result.github_before = github_before.clone();
    result.buzz_before = buzz_before.clone();
    result.github_target = request.expected_target.clone();
    result.buzz_after = if push_attempted { None } else { buzz_before };
    result.push_attempted = Some(push_attempted);
    result.mutation = if push_attempted {
        MutationEvidence::Unknown
    } else {
        MutationEvidence::None
    };
    result.retryable = retryable;
    result
}

fn mapped_error(error: &PortError) -> (Classification, bool, NextAction) {
    match error.kind() {
        PortErrorKind::Config => (
            Classification::ConfigError,
            false,
            NextAction::FixConfiguration,
        ),
        PortErrorKind::Auth => (
            Classification::AuthError,
            false,
            NextAction::RepairCredentialsOrAcl,
        ),
        PortErrorKind::Transient => (
            Classification::TransientError,
            true,
            NextAction::RetryWithBackoff,
        ),
        PortErrorKind::Verify | PortErrorKind::Unexpected => (
            Classification::VerifyError,
            false,
            NextAction::InspectUnexpectedFailure,
        ),
    }
}

fn base_result(
    run_id: RunId,
    enrollment: &Enrollment,
    request: &ReconcileRequest,
    classification: Classification,
    next_action: NextAction,
    summary: &str,
    retryable: bool,
) -> ReconcileResult {
    ReconcileResult {
        schema_version: 1,
        run_id,
        enrollment_id: enrollment.id.clone(),
        github_repository_id: enrollment.github_repository_id.clone(),
        managed_ref: enrollment.managed_ref.clone(),
        mode: request.mode,
        trigger: request.trigger,
        classification,
        github_before: None,
        buzz_before: None,
        github_target: request.expected_target.clone(),
        buzz_after: None,
        mutation: MutationEvidence::None,
        push_attempted: Some(false),
        approval_event: request.approval_event.clone(),
        retryable,
        next_action,
        summary: summary.to_owned(),
        outcome: ExitOutcome::Failure,
        exit_code: 1,
    }
}

fn succeed(result: &mut ReconcileResult, summary: &str) {
    result.next_action = NextAction::None;
    result.summary = summary.to_owned();
    result.outcome = ExitOutcome::Success;
    result.exit_code = 0;
}

fn fail(result: &mut ReconcileResult, action: NextAction, summary: &str) {
    result.next_action = action;
    result.summary = summary.to_owned();
    result.outcome = ExitOutcome::Failure;
    result.exit_code = 1;
}
