use super::CatalogProvider;
use aifuel_core::ProviderKey;

pub static DEFINITION: CatalogProvider =
    CatalogProvider::file_source(ProviderKey::Copilot, ".copilot/config.json");
