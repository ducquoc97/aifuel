use aifuel_core::ProviderKey;

/// One model advertised by a provider's native catalog interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCatalogModel {
    pub model_id: String,
    pub display_label: Option<String>,
    pub default_effort: Option<String>,
    /// `None` means the provider did not report model-specific effort data.
    /// An empty list means the provider explicitly reported no effort choices.
    pub supported_efforts: Option<Vec<String>>,
}

/// Outcome of consulting the provider catalog capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCatalogDiscovery {
    Available {
        integration_version: Option<String>,
        models: Vec<ProviderCatalogModel>,
    },
    Unsupported {
        diagnostic: String,
    },
    Failed {
        integration_version: Option<String>,
        diagnostic: String,
    },
}

/// Discover models only through a provider's dedicated catalog capability.
/// Providers without a verified interface are explicitly reported as
/// unsupported instead of receiving a guessed or hard-coded model list.
pub async fn discover_model_catalog(provider: ProviderKey) -> ProviderCatalogDiscovery {
    match provider {
        ProviderKey::Codex => match crate::codex::model_catalog::discover().await {
            Ok(catalog) => ProviderCatalogDiscovery::Available {
                integration_version: catalog.integration_version,
                models: catalog.models,
            },
            Err(error) => ProviderCatalogDiscovery::Failed {
                integration_version: error.integration_version,
                diagnostic: error.diagnostic,
            },
        },
        provider => ProviderCatalogDiscovery::Unsupported {
            diagnostic: format!(
                "model catalog discovery is not implemented for {provider}; no model list was inferred"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn providers_without_a_catalog_interface_return_explicit_unsupported() {
        for provider in [
            ProviderKey::Claude,
            ProviderKey::Copilot,
            ProviderKey::Gemini,
            ProviderKey::Antigravity,
        ] {
            let result = discover_model_catalog(provider).await;
            let ProviderCatalogDiscovery::Unsupported { diagnostic } = result else {
                panic!("{provider} must not receive a fabricated model catalog");
            };
            assert!(diagnostic.contains("not implemented"));
        }
    }
}
