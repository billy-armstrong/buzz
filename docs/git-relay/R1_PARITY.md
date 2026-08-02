# Git-relay R1 Rust parity

R1 ports the deterministic reconciliation behavior from the frozen TypeScript
prototype at `billy-armstrong/buzz-workspace#4`, commit
`83a03ad3941854c1912eecc9dda8983b12df3038`.

Rust is the only intended production implementation. The prototype remains a
behavioral oracle until native relay integration supersedes it; R1 does not
deploy or call it.

## Boundary

The crate exposes one behavioral operation:

```rust
Reconciler::reconcile(&Enrollment, ReconcileRequest) -> ReconcileResult
```

The reconciler owns ordering, classification, exact-target authorization,
single-flight acquisition, replay keys, cleanup, and truthful evidence. Host
adapters own repository access, persistence, identity/ACL resolution, and run
ID creation through the traits in `ports`.

R1 deliberately contains no GitHub client, Nostr event handling, Postgres,
webhook, scheduler, CLI, Desktop, or live-repository code. The disposable Git
adapter is test-only. Consequently, TypeScript tests that exercised YAML/JSON
or CLI mechanics map to the Rust core's typed/sanitized boundary contract;
native parsing and command integration remain R2 work.

## Oracle mapping

### Controller state machine (34 TypeScript tests)

| TypeScript oracle behavior | Rust public-seam test |
| --- | --- |
| Equal tips are in sync | `equal_tips_are_in_sync_without_mutation` |
| Observe reports relay behind | `observe_reports_relay_behind_without_mutation` |
| Approved exact apply | `approved_apply_fast_forwards_exact_ref_and_verifies` |
| GitHub behind freezes | `github_behind_freezes_without_mutation` |
| Diverged freezes | `diverged_histories_freeze_without_mutation` |
| Invalid configuration freezes | `invalid_enrollment_policy_returns_config_error` |
| Disabled repository is unmanaged | `disabled_enrollment_is_unmanaged_and_never_opens_repository` |
| Authentication failure | `authentication_failure_freezes_without_retry` |
| Temporary remote failure | `temporary_remote_failure_is_retryable_without_mutation` |
| Unexpected adapter failure is sanitized | `unexpected_adapter_failure_is_stable_and_secret_free` |
| Apply target must be fresh/exact | `apply_target_must_equal_fresh_github_tip` |
| Valid approval is required | `apply_requires_an_approval_event` |
| Equal-tip apply remains gated | `already_equal_apply_still_requires_exact_approval` |
| Concurrent relay-only change freezes | `concurrent_relay_only_change_is_reclassified_without_push` |
| Concurrent divergence freezes | `concurrent_divergent_change_is_reclassified_without_push` |
| GitHub advances before push | `github_change_before_push_freezes_old_approval` |
| Rejected push + unchanged Buzz means no mutation | `rejected_push_with_unchanged_buzz_records_no_mutation` |
| Ambiguous push verified at exact target | `ambiguous_push_is_resolved_by_exact_reread` |
| Attempted push cannot be reread | `attempted_push_with_unreadable_outcome_is_unknown` |
| Successful push cannot be reread | `successful_push_with_failed_reread_is_unknown` |
| Unexpected post-attempt boundary | `unexpected_failure_after_push_is_unknown_and_secret_free` |
| Unequal post-push refs | `successful_push_with_unequal_refs_is_verify_error` |
| GitHub advances after confirmed delivery | `github_advance_after_delivery_records_confirmed_fast_forward` |
| GitHub advances without delivery | `github_advance_with_unchanged_buzz_does_not_claim_delivery` |
| Exact verified replay is deduplicated | `duplicate_verified_apply_returns_prior_without_duplicate_evidence` |
| Replay cleanup describes current invocation | `cleanup_failure_during_replay_describes_current_no_push_invocation` |
| Observe evidence cannot satisfy apply | `observe_record_cannot_satisfy_later_apply` |
| New approval creates new evidence | `new_approval_gets_a_new_apply_record` |
| Repointed repository identity cannot replay | `repointed_repository_identity_cannot_reuse_old_evidence` |
| Busy lock is retryable | `busy_lock_is_retryable_and_does_not_open_repository` |
| Session close failure is structured | `session_close_failure_cannot_mask_machine_result` |
| Cleanup failure after push is unknown | `close_failure_after_push_preserves_unknown_mutation` |
| Lock release failure is structured | `release_failure_cannot_mask_machine_result` |
| Evidence append failure is structured | `audit_failure_returns_stable_secret_free_result` and `audit_failure_after_push_preserves_unknown_mutation` |

The Rust suite additionally exercises missing-ref boundary classification,
evidence lookup failure, ancestry failure, pre-push refresh failure, and
independent sequential invocations in `tests/reconcile.rs`.

### Configuration and enrollment (8 TypeScript tests)

The prototype accepted untrusted manifest/enrollment files inside its package.
R1 instead accepts a host-resolved typed `Enrollment`; the future relay adapter
must perform channel/repository/ACL lookup before constructing it.

| TypeScript oracle category | Rust equivalent |
| --- | --- |
| Matching manifest + host enrollment | `enrollment_defaults_to_observe_only_and_requires_approval` |
| Missing channel / redirected authority fail closed | typed `Enrollment` contains no URL or credential authority; `invalid_enrollment_policy_returns_config_error` proves invalid host policy freezes before repository open |
| Clone URLs with credentials are rejected | `serialized_enrollment_contains_no_credentials_or_clone_urls`; URLs are absent from the core API |
| GitHub identity mismatch is rejected | immutable typed `GithubRepositoryId`; `repointed_repository_identity_cannot_reuse_old_evidence` |
| Approval is mandatory | `enrollment_defaults_to_observe_only_and_requires_approval` and `apply_requires_an_approval_event` |
| Buzz owner/repository mismatch is rejected | host adapter responsibility; no owner/URL strings can enter the R1 state machine |
| Inert credential profiles are rejected | no credential-profile field or credential material exists in the R1 API |

The remaining value-validation cases are covered by all eight tests in
`tests/domain.rs`: opaque identity validation, exact 40-hex commit IDs, exact
64-hex approval IDs, the one-ref allowlist, observe-only defaults, exact apply
binding, and absence of credential/URL fields.

### Real Git transport (4 TypeScript tests)

All four behaviors run against disposable local bare repositories in
`tests/real_git.rs`:

| Oracle behavior | Rust test |
| --- | --- |
| Exact managed-ref fast-forward; unlisted ref preserved | `real_git_fast_forward_preserves_unlisted_branch` |
| Exact SHA-to-ref refspec only | `real_git_push_uses_only_exact_sha_to_exact_refspec` |
| Missing GitHub main creates nothing | `missing_github_main_never_creates_either_managed_ref` |
| Missing Buzz main creates nothing | `missing_buzz_main_never_creates_either_managed_ref` |

The adapter exists only in the integration test. R2 must send the same typed
operation through Buzz's existing receive-pack, policy-hook, and CAS manifest
publication path instead of adding another production ref writer.

### Lock, evidence, batch, and CLI categories (7 TypeScript tests)

| Prototype category | R1 equivalent |
| --- | --- |
| Lock acquisition/release | `busy_lock_is_retryable_and_does_not_open_repository`, `release_failure_cannot_mask_machine_result` |
| Persist and replay exact secret-free evidence | `duplicate_verified_apply_returns_prior_without_duplicate_evidence`, `repointed_repository_identity_cannot_reuse_old_evidence`, `serialized_enrollment_contains_no_credentials_or_clone_urls` |
| One repository failure does not stop another | `one_repository_failure_does_not_poison_a_later_invocation`; R1 intentionally has no shared batch operation |
| Invalid config emits nonzero machine result | `invalid_enrollment_policy_returns_config_error` |
| Unexpected outer boundary is secret-free | every `ReconcileResult` is serialized from a closed schema; unexpected adapter/cleanup/evidence tests assert sanitization |
| Audit reports independent enrollments | one `reconcile` call per enrollment; R2 scheduler owns iteration and persistence |
| Malformed job does not stop later job | malformed enrollment never enters the typed core; `one_repository_failure_does_not_poison_a_later_invocation` proves invocation isolation |

## R1 acceptance evidence

- Exactly 53 Rust tests cover the mapped prototype matrix: 41 reconciliation,
  8 domain, and 4 disposable-real-Git tests.
- `ManagedRef` accepts only `refs/heads/main`.
- `CommitOid` and `ApprovalEventId` reject malformed values.
- Apply requires Phase 2, host apply enablement, the freshly observed exact
  GitHub SHA, and an approval event.
- The replay key includes enrollment ID, immutable GitHub repository ID, exact
  ref, exact target, and exact approval event.
- Any unverified failure after `push_exact` sets `mutation = unknown` and
  clears `buzz_after`.
- Missing managed refs are configuration errors; neither real-Git test creates
  them.
- The real-Git fast-forward test proves an unlisted branch remains unchanged.
- Boundary errors have only a closed category and cannot carry raw adapter
  messages into results.
- R1 production sources contain no `unsafe`, `unwrap`, or `expect`.

Static safety scan:

```sh
rg -n --glob '*.rs' '(--mirror|--prune|force-with-lease|force_push|wildcard|delete-ref|refs/heads/\*)' crates/buzz-git-relay/src
rg -n '\.(unwrap|expect)\(' crates/buzz-git-relay/src
rg -n 'unsafe\s*\{' crates/buzz-git-relay/src
```

All three searches must be empty. The test-only Git adapter contains the
literal exact destination `refs/heads/main` and uses a leading `+` only for
local fetch scratch refs; its push refspec is always `<validated SHA>:<typed
ManagedRef>` and never contains `+`, a wildcard, deletion, or mirror option.
