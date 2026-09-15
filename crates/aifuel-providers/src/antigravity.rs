use super::{DiscoveryContext, ProviderAdapter, ProviderDefinition, initialized_adapter};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderDescriptor, ProviderKey};

pub struct AntigravityDefinition;

pub static DEFINITION: AntigravityDefinition = AntigravityDefinition;

impl ProviderDefinition for AntigravityDefinition {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::for_key(ProviderKey::Antigravity)
    }

    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
        context.inspect_any(&[".gemini/antigravity", ".gemini/antigravity-cli"])
    }

    fn initialize(&self) -> Box<dyn ProviderAdapter> {
        initialized_adapter(self.descriptor())
    }
}
