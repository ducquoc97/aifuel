use super::discovery::SourceKind;
use super::{CatalogProvider, DiscoveryContext};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderKey};

fn discover(context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
    context.inspect_source(".claude/.credentials.json", SourceKind::File)
}

pub static DEFINITION: CatalogProvider = CatalogProvider::new(ProviderKey::Claude, discover);
