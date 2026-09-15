use super::{DiscoveryContext, ProviderAdapter, ProviderDefinition, initialized_adapter};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderDescriptor, ProviderKey};

pub struct ClaudeDefinition;

pub static DEFINITION: ClaudeDefinition = ClaudeDefinition;

impl ProviderDefinition for ClaudeDefinition {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::for_key(ProviderKey::Claude)
    }

    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
        context.inspect_source(".claude/.credentials.json")
    }

    fn initialize(&self) -> Box<dyn ProviderAdapter> {
        initialized_adapter(self.descriptor())
    }
}
