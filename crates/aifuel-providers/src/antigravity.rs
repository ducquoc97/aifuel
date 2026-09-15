use super::CatalogProvider;
use aifuel_core::ProviderKey;

pub static DEFINITION: CatalogProvider = CatalogProvider::directory_sources(
    ProviderKey::Antigravity,
    &[".gemini/antigravity", ".gemini/antigravity-cli"],
);
