use super::CatalogProvider;
use aifuel_core::ProviderKey;

pub static DEFINITION: CatalogProvider =
    CatalogProvider::file_source(ProviderKey::Codex, ".codex/auth.json");
