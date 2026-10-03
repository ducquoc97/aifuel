//! Named route resolution for the `/v1` gateway: model aliases and
//! ordered combos from `gateway.json` in the config dir. Stub: the
//! implementing agent fills in loading and resolution.

/// What a configured name resolves to when an inbound `model` string
/// matches it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Resolution {
    /// Rewrite the selector to another model string and re-resolve.
    Alias(String),
    /// An ordered chain of selectors to attempt in place of `auto`.
    Combo(Vec<String>),
}

/// Resolve `model` against configured aliases and combos. `Ok(None)`
/// means no configured name matched and the caller falls back to the
/// built-in addressing convention.
pub(crate) fn resolve(_model: &str) -> Result<Option<Resolution>, String> {
    Ok(None)
}
