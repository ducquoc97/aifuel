use super::discovery::SourceKind;
use super::{CatalogProvider, DiscoveryContext};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderKey};

fn discover(context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
    // GitHub CLI credentials and generic GitHub token environment variables
    // are intentionally outside this provider-owned source check.
    context.inspect_source(".copilot/config.json", SourceKind::File)
}

pub static DEFINITION: CatalogProvider = CatalogProvider::new(ProviderKey::Copilot, discover);
