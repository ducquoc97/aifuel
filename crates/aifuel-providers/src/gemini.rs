use super::{DiscoveryContext, ProviderAdapter, ProviderDefinition, initialized_adapter};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderDescriptor, ProviderKey};

pub struct GeminiDefinition;

pub static DEFINITION: GeminiDefinition = GeminiDefinition;

impl ProviderDefinition for GeminiDefinition {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::for_key(ProviderKey::Gemini)
    }

    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
        context.inspect_source(".gemini/oauth_creds.json")
    }

    fn initialize(&self) -> Box<dyn ProviderAdapter> {
        initialized_adapter(self.descriptor())
    }
}
