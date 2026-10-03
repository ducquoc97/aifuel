//! Token-optimization engines applied to run payloads.
//!
//! The plan a caller configures (`stack` order plus a level per engine)
//! rides [`crate::RunRequest::optimize`]; the engines are deterministic,
//! fail-open transforms ported from two external projects:
//!
//! - **rtk** ([`rtk::RtkLevel`]): structural compression for command and
//!   tool output - dedupe, truncation, per-kind filters - after RTK
//!   (Rust Token Killer).
//! - **caveman** ([`caveman::CavemanLevel`]): prose condensation and a
//!   terse-response system instruction, after the Caveman skill.
//!
//! Both engines share one rule: when a transform cannot beat the raw
//! bytes it returns them unchanged, so enabling a plan can only ever
//! shrink or pass through a payload.

mod caveman;
mod detect;
mod rtk;

pub use caveman::CavemanLevel;
pub use detect::PayloadKind;
pub use rtk::RtkLevel;

/// One optimizer engine, named in `stack` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizerKind {
    /// Structural output compression (RTK port).
    Rtk,
    /// Prose condensation + terse instruction (Caveman port).
    Caveman,
}

impl OptimizerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rtk => "rtk",
            Self::Caveman => "caveman",
        }
    }
}

/// The optimization plan one run carries: which engines run, in which
/// order, at which level. The default is both engines off - a request
/// that never configured a plan is byte-identical to before the feature.
///
/// The `providers.json` spelling:
///
/// ```json
/// "optimizer": {
///   "stack": ["rtk", "caveman"],
///   "rtk":     { "level": "standard" },
///   "caveman": { "level": "full" }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OptimizePlan {
    /// Pipeline order; engines absent from `stack` do not run even when
    /// their level is on.
    #[serde(default = "default_stack")]
    pub stack: Vec<OptimizerKind>,
    /// `rtk` engine level: `off` | `standard` | `ultra`. The file accepts
    /// both `"rtk": "standard"` and `"rtk": {"level": "standard"}`.
    #[serde(default, deserialize_with = "rtk_level_field")]
    pub rtk: RtkLevel,
    /// `caveman` engine level: `off` | `lite` | `full` | `ultra`, same
    /// two spellings as `rtk`.
    #[serde(default, deserialize_with = "caveman_level_field")]
    pub caveman: CavemanLevel,
}

fn default_stack() -> Vec<OptimizerKind> {
    vec![OptimizerKind::Rtk, OptimizerKind::Caveman]
}

fn rtk_level_field<'de, D>(deserializer: D) -> Result<RtkLevel, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Level(RtkLevel),
        Nested { level: RtkLevel },
    }
    Ok(match Repr::deserialize(deserializer)? {
        Repr::Level(level) | Repr::Nested { level } => level,
    })
}

fn caveman_level_field<'de, D>(deserializer: D) -> Result<CavemanLevel, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Level(CavemanLevel),
        Nested { level: CavemanLevel },
    }
    Ok(match Repr::deserialize(deserializer)? {
        Repr::Level(level) | Repr::Nested { level } => level,
    })
}

impl Default for OptimizePlan {
    fn default() -> Self {
        Self {
            stack: default_stack(),
            rtk: RtkLevel::Off,
            caveman: CavemanLevel::Off,
        }
    }
}

impl OptimizePlan {
    /// Whether any configured engine would transform a payload.
    pub fn is_active(&self) -> bool {
        self.stack.iter().any(|kind| match kind {
            OptimizerKind::Rtk => self.rtk != RtkLevel::Off,
            OptimizerKind::Caveman => self.caveman != CavemanLevel::Off,
        })
    }

    /// Run the stack over one payload: classify once, apply each engine
    /// in order, then enforce the never-worse guard. Both engines are
    /// fail-open internally; this is the last line of the same contract.
    pub fn apply(&self, payload: &str) -> String {
        if !self.is_active() {
            return payload.to_owned();
        }
        let kind = detect::detect(payload);
        let mut text = payload.to_owned();
        for engine in &self.stack {
            text = match engine {
                OptimizerKind::Rtk => rtk::compress(&text, kind, self.rtk),
                OptimizerKind::Caveman => caveman::compress(&text, kind, self.caveman),
            };
        }
        rtk::guarded(payload, text)
    }

    /// The terse-response system instruction for wire integrations, or
    /// `None` when `caveman` is off or absent from `stack`.
    pub fn caveman_instruction(&self) -> Option<&'static str> {
        if !self.stack.contains(&OptimizerKind::Caveman) {
            return None;
        }
        caveman::instruction(self.caveman)
    }

    /// Apply one `engine:level` CLI override on top of `self` (config
    /// file first, flags last). The named engine enters `stack` when it
    /// was absent - spelling `--optimize rtk:standard` explicitly opts
    /// the engine in.
    pub fn with_override(&self, spec: &str) -> Result<Self, String> {
        let (engine, level) = spec.split_once(':').ok_or_else(|| {
            format!("--optimize expects engine:level (for example rtk:ultra), got {spec:?}")
        })?;
        let mut plan = self.clone();
        let kind = match engine {
            "rtk" => OptimizerKind::Rtk,
            "caveman" => OptimizerKind::Caveman,
            other => {
                return Err(format!(
                    "unknown optimizer {other:?}; known values: rtk, caveman"
                ));
            }
        };
        match kind {
            OptimizerKind::Rtk => {
                plan.rtk = RtkLevel::parse(level).ok_or_else(|| {
                    format!("unknown rtk level {level:?}; known values: off, standard, ultra")
                })?;
            }
            OptimizerKind::Caveman => {
                plan.caveman = CavemanLevel::parse(level).ok_or_else(|| {
                    format!("unknown caveman level {level:?}; known values: off, lite, full, ultra")
                })?;
            }
        }
        if !plan.stack.contains(&kind) {
            plan.stack.push(kind);
        }
        Ok(plan)
    }

    /// A one-line description for stderr narration, e.g.
    /// `rtk:standard+caveman:full`; `off` when nothing is active.
    pub fn describe(&self) -> String {
        if !self.is_active() {
            return "off".to_owned();
        }
        self.stack
            .iter()
            .map(|kind| {
                let level = match kind {
                    OptimizerKind::Rtk => self.rtk.as_str(),
                    OptimizerKind::Caveman => self.caveman.as_str(),
                };
                format!("{}:{level}", kind.as_str())
            })
            .collect::<Vec<_>>()
            .join("+")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plan_is_inert() {
        let plan = OptimizePlan::default();
        assert!(!plan.is_active());
        let payload = "just actually a whole bunch of filler text";
        assert_eq!(plan.apply(payload), payload);
        assert_eq!(plan.describe(), "off");
    }

    #[test]
    fn apply_runs_stack_in_order_and_never_grows() {
        let plan = OptimizePlan {
            stack: default_stack(),
            rtk: RtkLevel::Ultra,
            caveman: CavemanLevel::Full,
        };
        let log_like = "2026-01-01T00:00 INFO line one\n2026-01-01T00:01 INFO line two\n2026-01-01T00:02 ERROR real failure\n2026-01-01T00:03 INFO line four\n";
        let out = plan.apply(log_like);
        assert!(out.contains("ERROR real failure"));
        assert!(out.len() <= log_like.len());
    }

    #[test]
    fn overrides_parse_and_opt_the_engine_in() {
        let base = OptimizePlan::default();
        let plan = base.with_override("rtk:ultra").expect("valid spec");
        assert_eq!(plan.rtk, RtkLevel::Ultra);
        assert!(plan.stack.contains(&OptimizerKind::Rtk));
        assert!(base.with_override("bogus:ultra").is_err());
        assert!(base.with_override("rtk:bogus").is_err());
        assert!(base.with_override("rtk").is_err());
    }

    #[test]
    fn instruction_only_when_caveman_is_stacked_and_on() {
        let mut plan = OptimizePlan {
            caveman: CavemanLevel::Full,
            ..OptimizePlan::default()
        };
        assert!(plan.caveman_instruction().is_some());
        plan.stack = vec![OptimizerKind::Rtk];
        assert!(plan.caveman_instruction().is_none());
    }

    #[test]
    fn providers_json_shape_decodes() {
        let plan: OptimizePlan = serde_json::from_str(
            r#"{"stack": ["rtk", "caveman"], "rtk": {"level": "standard"}, "caveman": {"level": "full"}}"#,
        )
        .expect("the documented shape decodes");
        assert_eq!(plan.rtk, RtkLevel::Standard);
        assert_eq!(plan.caveman, CavemanLevel::Full);
    }
}
