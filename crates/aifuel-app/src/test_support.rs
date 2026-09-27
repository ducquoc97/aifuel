//! Helpers shared across the crate's test modules.

/// A unique temporary database path for one test.
pub(crate) fn store_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "aifuel-test-store-{label}-{}-{}.db",
        std::process::id(),
        crate::run_management::now().to_bits()
    ))
}
