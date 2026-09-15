use super::discovery::SourceKind;
use super::{CatalogProvider, DiscoveryContext};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderKey};

fn discover(context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
    context.inspect_source(".gemini/oauth_creds.json", SourceKind::File)
}

pub static DEFINITION: CatalogProvider = CatalogProvider::new(ProviderKey::Gemini, discover);
