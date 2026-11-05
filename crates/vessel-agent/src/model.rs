//! Choosing the requested model among an agent's session config options.
//!
//! [Decision Log]
//! - Purpose: `SessionMode::Structured { model }` must select a model for
//!   any harness.
//! - Alternatives: per-harness CLI flags or environment variables (they
//!   differ per harness, and `opencode acp` has none); ACP's unstable
//!   `session/set_model` (behind an unstable crate feature that may change
//!   in any minor release).
//! - Choice: the stable ACP session config option whose category is
//!   `model`, set with `session/set_config_option`, as Paseo does. The
//!   requested name is matched against option values exactly, then without
//!   regard to case against values and display names.
//! - Failure is loud: a model the agent does not offer fails the session
//!   instead of silently running the default (which may be a far more
//!   expensive model than the one asked for).

use std::fmt;

use agent_client_protocol::v1::schema as acp;
use vessel_proto::ModelId;

/// The config change that selects a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub(crate) config: acp::SessionConfigId,
    pub(crate) value: acp::SessionConfigValueId,
}

/// Why the requested model cannot be selected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ModelError {
    /// The agent offers no model config option.
    #[error("the agent offers no model selection; cannot select `{wanted}`")]
    NoModelOption { wanted: ModelId },
    /// The agent offers models, but not this one.
    #[error(
        "the agent does not offer model `{wanted}` (offered: {})",
        Offered(offered)
    )]
    NotOffered {
        wanted: ModelId,
        offered: Vec<acp::SessionConfigValueId>,
    },
}

/// The change needed to run `wanted`, or `None` if it is already current.
pub(crate) fn choose(
    options: &[acp::SessionConfigOption],
    wanted: &ModelId,
) -> Result<Option<ModelChoice>, ModelError> {
    let Some((config, select)) = options.iter().find_map(|option| {
        let is_model = matches!(
            option.category,
            Some(acp::SessionConfigOptionCategory::Model)
        );
        match &option.kind {
            acp::SessionConfigKind::Select(select) if is_model => Some((&option.id, select)),
            acp::SessionConfigKind::Select(_) | acp::SessionConfigKind::Boolean(_) | _ => None,
        }
    }) else {
        return Err(ModelError::NoModelOption {
            wanted: wanted.clone(),
        });
    };
    let choices = flatten(&select.options);
    let name = wanted.as_str();
    let found = choices
        .iter()
        .find(|choice| &*choice.value.0 == name)
        .or_else(|| {
            choices.iter().find(|choice| {
                choice.value.0.eq_ignore_ascii_case(name) || choice.name.eq_ignore_ascii_case(name)
            })
        });
    let Some(found) = found else {
        return Err(ModelError::NotOffered {
            wanted: wanted.clone(),
            offered: choices.iter().map(|choice| choice.value.clone()).collect(),
        });
    };
    if found.value == select.current_value {
        return Ok(None);
    }
    Ok(Some(ModelChoice {
        config: config.clone(),
        value: found.value.clone(),
    }))
}

fn flatten(options: &acp::SessionConfigSelectOptions) -> Vec<&acp::SessionConfigSelectOption> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => Vec::new(),
    }
}

/// Offered values, comma-separated, for the error message.
struct Offered<'a>(&'a [acp::SessionConfigValueId]);

impl fmt::Display for Offered<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, value) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&value.0)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(current: &str) -> Vec<acp::SessionConfigOption> {
        vec![
            acp::SessionConfigOption::select(
                "mode",
                "Mode",
                "default",
                vec![acp::SessionConfigSelectOption::new("default", "Default")],
            )
            .category(acp::SessionConfigOptionCategory::Mode),
            acp::SessionConfigOption::select(
                "model",
                "Model",
                current.to_owned(),
                vec![
                    acp::SessionConfigSelectOption::new("default", "Default (recommended)"),
                    acp::SessionConfigSelectOption::new("haiku", "Haiku"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ]
    }

    #[test]
    fn picks_the_model_option_by_value_then_by_name() {
        let choice = choose(&models("default"), &ModelId::new("haiku")).unwrap();
        assert_eq!(
            choice,
            Some(ModelChoice {
                config: "model".into(),
                value: "haiku".into(),
            })
        );
        let by_name = choose(&models("default"), &ModelId::new("HAIKU")).unwrap();
        assert_eq!(by_name.map(|choice| choice.value), Some("haiku".into()));
    }

    #[test]
    fn the_current_model_needs_no_change() {
        assert_eq!(choose(&models("haiku"), &ModelId::new("haiku")), Ok(None));
    }

    #[test]
    fn unknown_models_and_missing_options_are_errors() {
        let error = choose(&models("default"), &ModelId::new("opus")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the agent does not offer model `opus` (offered: default, haiku)"
        );
        assert!(matches!(
            choose(&[], &ModelId::new("opus")),
            Err(ModelError::NoModelOption { .. })
        ));
    }
}
