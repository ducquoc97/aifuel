//! The built-in browser-session Provider Integrations.
//!
//! A `*:web` integration binds pasted browser-session material - a session
//! token or a copied `Cookie` header line - delivered as the `Cookie`
//! header, never `Authorization: Bearer`. It exists to carry its Monitoring
//! Collection Contract: the provider's web surface has no compiled
//! execution engine, so the declared Wire Api is protocol evidence naming
//! the upstream shape, not an executable surface. Material arrives only
//! through `aifuel auth set-session` (paste or stdin) or the declared
//! environment variable - nothing reads a browser profile or an OS keyring.

use crate::integrations::{EvidenceSource, IntegrationDescriptor};
use aifuel_core::{
    ApiKeySource, AuthBinding, CollectorId, CredentialRef, EndpointConfig, ExecutionConfig,
    Integration, IntegrationId, KeyDelivery, MonitoringConfig, ProviderId, WireApi,
};
use std::collections::BTreeMap;

/// The session-cookie integrations, currently the single `claude-web:web`
/// demonstrator OmniRoute's `claude_web` provider documents.
///
/// Claude accepts a full `Cookie` header or the bare `sessionKey` value;
/// `KeyDelivery::Cookie` normalizes both at send time. The `EnvOrStore`
/// source keeps the `CLAUDE_WEB_SESSION` variable working as-is while
/// `aifuel auth set-session claude-web:web` stores the managed credential
/// that then takes precedence.
pub(super) fn integrations() -> Vec<IntegrationDescriptor> {
    vec![IntegrationDescriptor::builtin(
        Integration {
            id: IntegrationId::new("claude-web:web"),
            provider: ProviderId::new("claude-web"),
            name: "Claude (web session)".to_owned(),
            execution: ExecutionConfig::Http {
                endpoint: EndpointConfig {
                    base_url: "https://claude.ai".to_owned(),
                    extra_headers: BTreeMap::new(),
                    request_timeout_seconds: None,
                },
                protocol: WireApi::ClaudeWeb,
                auth: AuthBinding::ApiKey {
                    source: ApiKeySource::EnvOrStore {
                        var: "CLAUDE_WEB_SESSION".to_owned(),
                        credential: CredentialRef::new("claude-web:web"),
                    },
                    delivery: KeyDelivery::Cookie {
                        name: "sessionKey".to_owned(),
                    },
                },
            },
            monitoring: Some(MonitoringConfig {
                collector: CollectorId::new(crate::claude_web::CLAUDE_WEB_USAGE_COLLECTOR),
                credential: None,
                endpoint: None,
            }),
        },
        vec![
            EvidenceSource::EnvVar("CLAUDE_WEB_SESSION".to_owned()),
            EvidenceSource::ManagedEntry(CredentialRef::new("claude-web:web")),
        ],
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_web_integration_binds_a_session_credential_for_monitoring() {
        let descriptors = integrations();
        let descriptor = descriptors
            .iter()
            .find(|d| d.integration.id.as_str() == "claude-web:web")
            .expect("claude-web:web is a builtin");
        let ExecutionConfig::Http {
            endpoint,
            protocol,
            auth,
        } = &descriptor.integration.execution
        else {
            panic!("claude-web:web must be an Http integration");
        };
        assert_eq!(endpoint.base_url, "https://claude.ai");
        // The web protocol is declared as evidence; no engine serves it, so
        // the integration can never execute a prompt - only collect.
        assert_eq!(*protocol, WireApi::ClaudeWeb);
        assert!(!crate::wire::serves(*protocol));
        assert_eq!(
            *auth,
            AuthBinding::ApiKey {
                source: ApiKeySource::EnvOrStore {
                    var: "CLAUDE_WEB_SESSION".to_owned(),
                    credential: CredentialRef::new("claude-web:web"),
                },
                delivery: KeyDelivery::Cookie {
                    name: "sessionKey".to_owned(),
                },
            },
            "a web session must deliver as a cookie, never a Bearer token"
        );
        let monitoring = descriptor
            .integration
            .monitoring
            .as_ref()
            .expect("claude-web:web declares a monitoring contract");
        assert_eq!(
            monitoring.collector.as_str(),
            crate::claude_web::CLAUDE_WEB_USAGE_COLLECTOR
        );
        assert!(monitoring.credential.is_none());
    }
}
