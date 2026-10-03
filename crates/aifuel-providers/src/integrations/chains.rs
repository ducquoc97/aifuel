//! Named fallback chains: the `chains` map inside `providers.json`.
//!
//! A chain is a user-configured ordered fallback list over registered
//! Provider Integrations - the configurable form of the built-in `auto`
//! route. `aifuel run --chain main` walks the steps in order and falls
//! through on the same pre-execution verdicts `auto` uses (unsupported
//! integration, launch failure, provider-reported quota exhaustion);
//! a step that started executing ends the walk.
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "chains": {
//!     "main": {
//!       "strategy": "priority",
//!       "steps": [
//!         { "integration": "claude" },
//!         { "integration": "codex" },
//!         { "integration": "glm:api-key", "model": "glm-4.7" }
//!       ]
//!     }
//!   }
//! }
//! ```
//!
//! - `strategy` is `"priority"`: try each step in declared order.
//! - `steps[].integration` names a registered Integration Identity -
//!   either a base integration or an instance selector id.
//! - `steps[].model` optionally overrides the run's model for that step.

use super::config::ConfigError;
use aifuel_core::IntegrationId;
use std::fmt;

/// The fallback order a chain applies. `priority` - walk the steps in
/// declared order - is the only strategy this build serves; the field is
/// decoded now so the file shape does not migrate when more strategies
/// (weighted, round-robin) arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainStrategy {
    /// Try each step in declared order; fall through on the shared
    /// pre-execution verdicts.
    Priority,
}

impl ChainStrategy {
    /// The config-file spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Priority => "priority",
        }
    }
}

/// One chain step: which integration to run, and optionally which model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainStep {
    /// The registered Integration Identity this step selects. An instance
    /// selector id is legal - the serving lookup resolves it to its base.
    pub integration: IntegrationId,
    /// A per-step model override; `None` keeps the run's `--model`.
    pub model: Option<String>,
}

/// One named fallback chain, decoded from `providers.json` and checked
/// against the registry at build time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainDescriptor {
    /// The chain's name (the `chains` map key): what `--chain` selects.
    pub name: String,
    /// The order strategy applied across `steps`.
    pub strategy: ChainStrategy,
    /// The ordered fallback targets; never empty after validation.
    pub steps: Vec<ChainStep>,
}

impl fmt::Display for ChainDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let steps = self
            .steps
            .iter()
            .map(|step| match &step.model {
                Some(model) => format!("{} ({model})", step.integration),
                None => step.integration.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" -> ");
        write!(f, "{} [{}]: {steps}", self.name, self.strategy.as_str())
    }
}

/// The `chains` map value.
#[derive(serde::Deserialize)]
struct ConfigChain {
    strategy: String,
    steps: Vec<ConfigStep>,
}

/// One `steps` entry: `integration` required, `model` optional.
#[derive(serde::Deserialize)]
struct ConfigStep {
    integration: String,
    #[serde(default)]
    model: Option<String>,
}

/// Validate one `chains` map entry into a [`ChainDescriptor`]. Shape
/// errors only - whether the named integrations exist is the registry's
/// job, the same split `build_instance` uses.
pub(super) fn build_chain(
    name: &str,
    raw: serde_json::Value,
) -> Result<ChainDescriptor, ConfigError> {
    let invalid = |reason: String| ConfigError::InvalidChain {
        name: name.to_owned(),
        reason,
    };
    if name.trim().is_empty() {
        return Err(invalid("chain name must not be empty".to_owned()));
    }
    let entry: ConfigChain = serde_json::from_value(raw)
        .map_err(|error| invalid(super::config::describe_json_error(&error)))?;
    let strategy = match entry.strategy.trim() {
        "priority" => ChainStrategy::Priority,
        other => {
            return Err(invalid(format!(
                "chain strategy '{other}' is unknown; known values: priority"
            )));
        }
    };
    if entry.steps.is_empty() {
        return Err(invalid("a chain needs at least one step".to_owned()));
    }
    let steps = entry
        .steps
        .into_iter()
        .enumerate()
        .map(|(index, step)| {
            if step.integration.trim().is_empty() {
                return Err(invalid(format!(
                    "steps[{index}].integration must not be empty"
                )));
            }
            Ok(ChainStep {
                integration: IntegrationId::new(step.integration),
                model: step.model.filter(|model| !model.trim().is_empty()),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChainDescriptor {
        name: name.to_owned(),
        strategy,
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_chain_decodes_with_model_override() {
        let chain = build_chain(
            "main",
            serde_json::json!({
                "strategy": "priority",
                "steps": [
                    { "integration": "claude" },
                    { "integration": "glm:api-key", "model": "glm-4.7" }
                ]
            }),
        )
        .expect("a documented chain decodes");
        assert_eq!(chain.strategy, ChainStrategy::Priority);
        assert_eq!(chain.steps.len(), 2);
        assert_eq!(chain.steps[0].integration.as_str(), "claude");
        assert_eq!(chain.steps[1].model.as_deref(), Some("glm-4.7"));
    }

    #[test]
    fn unknown_strategy_and_empty_steps_fail_explicitly() {
        let bad_strategy = build_chain(
            "x",
            serde_json::json!({"strategy": "weighted", "steps": [{"integration": "a"}]}),
        );
        assert!(matches!(
            bad_strategy,
            Err(ConfigError::InvalidChain { .. })
        ));
        let empty = build_chain(
            "x",
            serde_json::json!({"strategy": "priority", "steps": []}),
        );
        assert!(matches!(empty, Err(ConfigError::InvalidChain { .. })));
        let no_integration = build_chain(
            "x",
            serde_json::json!({"strategy": "priority", "steps": [{"model": "m"}]}),
        );
        assert!(matches!(
            no_integration,
            Err(ConfigError::InvalidChain { .. })
        ));
    }
}
