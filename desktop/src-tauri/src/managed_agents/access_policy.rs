//! Distribution policy at managed-agent enforcement boundaries.

use super::{validate_respond_to_allowlist, ManagedAgentRecord, RespondTo};

pub(crate) type RespondToEnv = (Vec<(&'static str, String)>, Vec<&'static str>);

/// Internal packaging sets `BUZZ_BUILD_INTERNAL`; OSS/custom builds do not.
pub(crate) fn internal_build() -> bool {
    option_env!("BUZZ_DESKTOP_BUILD_INTERNAL").is_some()
}

pub(crate) fn owner_only() -> bool {
    owner_only_with_policy(internal_build())
}

pub(crate) fn owner_only_with_policy(internal: bool) -> bool {
    internal
}

/// Project effective access at a behavioral boundary without changing the
/// stored or relay-advertised access fields.
pub(crate) fn projected_access_with_policy(
    record: &ManagedAgentRecord,
    internal: bool,
) -> (RespondTo, Vec<String>) {
    if owner_only_with_policy(internal) {
        (RespondTo::OwnerOnly, Vec::new())
    } else {
        (record.respond_to, record.respond_to_allowlist.clone())
    }
}

/// Build the inbound-author access environment for a launched agent. The
/// explicit policy input keeps internal-build enforcement testable without
/// weakening the production caller's compile-time decision.
pub(crate) fn build_respond_to_env_with_policy(
    record: &ManagedAgentRecord,
    owner_hex: Option<&str>,
    enforced_owner_only: bool,
) -> Result<RespondToEnv, String> {
    let (respond_to, normalized) = projected_access_with_policy(record, enforced_owner_only);
    let normalized = if enforced_owner_only {
        normalized
    } else {
        validate_respond_to_allowlist(&normalized)?
    };
    if respond_to == RespondTo::Allowlist && normalized.is_empty() {
        return Err(
            "respond-to mode 'allowlist' requires at least one pubkey in the allowlist".to_string(),
        );
    }

    let mut set = vec![("BUZZ_ACP_RESPOND_TO", respond_to.as_str().to_string())];
    let mut remove = Vec::new();
    if enforced_owner_only {
        set.push((
            "BUZZ_ACP_ALLOWED_RESPOND_TO",
            RespondTo::OwnerOnly.as_str().to_string(),
        ));
    } else {
        remove.push("BUZZ_ACP_ALLOWED_RESPOND_TO");
    }
    if respond_to == RespondTo::Allowlist {
        set.push(("BUZZ_ACP_RESPOND_TO_ALLOWLIST", normalized.join(",")));
    } else {
        remove.push("BUZZ_ACP_RESPOND_TO_ALLOWLIST");
    }

    if record.auth_tag.is_none() {
        if let Some(owner) = owner_hex {
            set.push(("BUZZ_ACP_AGENT_OWNER", owner.to_string()));
        } else {
            remove.push("BUZZ_ACP_AGENT_OWNER");
        }
    } else {
        remove.push("BUZZ_ACP_AGENT_OWNER");
    }
    Ok((set, remove))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_agents::BackendKind;

    fn record(backend: BackendKind) -> ManagedAgentRecord {
        let mut record: ManagedAgentRecord = serde_json::from_value(serde_json::json!({
            "pubkey": "agent", "name": "Agent", "relay_url": "", "acp_command": "",
            "agent_command": "", "agent_args": [], "mcp_command": "",
            "turn_timeout_seconds": 0, "system_prompt": null, "created_at": "",
            "updated_at": "", "last_started_at": null, "last_stopped_at": null,
            "last_exit_code": null, "last_error": null
        }))
        .unwrap();
        record.backend = backend;
        record.respond_to = RespondTo::Anyone;
        record.respond_to_allowlist = vec!["malformed stale allowlist".into()];
        record
    }

    #[test]
    fn internal_enforcement_clamps_local_and_provider() {
        for (label, backend) in [
            ("local", BackendKind::Local),
            (
                "provider",
                BackendKind::Provider {
                    id: "p".into(),
                    config: serde_json::json!({}),
                },
            ),
        ] {
            let record = record(backend);
            let (gate_set, _) =
                build_respond_to_env_with_policy(&record, Some("owner"), true).unwrap();
            let gate_set: std::collections::HashMap<_, _> = gate_set.into_iter().collect();
            assert_eq!(
                gate_set.get("BUZZ_ACP_RESPOND_TO").map(String::as_str),
                Some("owner-only"),
                "internal runtime env did not clamp {label} agent",
            );
            assert_eq!(
                gate_set
                    .get("BUZZ_ACP_ALLOWED_RESPOND_TO")
                    .map(String::as_str),
                Some("owner-only"),
                "internal runtime env omitted the {label} agent guard",
            );

            let (respond_to, allowlist) = projected_access_with_policy(&record, true);
            assert_eq!(
                respond_to,
                RespondTo::OwnerOnly,
                "internal provider payload did not clamp {label} agent",
            );
            assert!(
                allowlist.is_empty(),
                "internal provider payload retained {label} agent allowlist",
            );
        }
    }
}
