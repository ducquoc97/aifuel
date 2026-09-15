use super::{DiscoveryContext, ProviderAdapter, ProviderDefinition, initialized_adapter};
use aifuel_core::{DiscoveryError, DiscoveryState, ProviderDescriptor, ProviderKey};

pub struct CopilotDefinition;

pub static DEFINITION: CopilotDefinition = CopilotDefinition;

impl ProviderDefinition for CopilotDefinition {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::for_key(ProviderKey::Copilot)
    }

    fn discover(&self, context: &DiscoveryContext) -> Result<DiscoveryState, DiscoveryError> {
        // GitHub CLI credentials and generic GitHub token environment variables
        // are intentionally outside this provider-owned source check.
        context.inspect_source(".copilot/config.json")
    }

    fn initialize(&self) -> Box<dyn ProviderAdapter> {
        initialized_adapter(self.descriptor())
    }
}
