use super::{DiscoveryContext, ProviderAdapter, ProviderDefinition, initialized_adapter};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderDescriptor, ProviderKey};

pub struct CodexDefinition;

pub static DEFINITION: CodexDefinition = CodexDefinition;

impl ProviderDefinition for CodexDefinition {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::for_key(ProviderKey::Codex)
    }

    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
        context.inspect_source(".codex/auth.json")
    }

    fn initialize(&self) -> Box<dyn ProviderAdapter> {
        initialized_adapter(self.descriptor())
    }
}
