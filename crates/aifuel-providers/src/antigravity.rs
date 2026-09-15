use super::discovery::SourceKind;
use super::{CatalogProvider, DiscoveryContext};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderKey};

fn discover(context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
    context.inspect_any(&[
        (".gemini/antigravity", SourceKind::Directory),
        (".gemini/antigravity-cli", SourceKind::Directory),
    ])
}

pub static DEFINITION: CatalogProvider = CatalogProvider::new(ProviderKey::Antigravity, discover);
