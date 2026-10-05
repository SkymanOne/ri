//! Choosing a model from `--model`, settings and available credentials.
//!
//! Port of `packages/coding-agent/src/core/model-resolver.ts` in pi `v1.0.0`.

use yapi_ai::registry::ModelRegistry;
use yapi_types::message::ThinkingLevel;
use yapi_types::model::Model;

/// The model pi picks for a provider when nothing else decides.
pub const DEFAULT_MODEL_PER_PROVIDER: &[(&str, &str)] = &[
    ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", "claude-opus-4-8"),
    ("openai", "gpt-5.5"),
    ("azure-openai-responses", "gpt-5.4"),
    ("openai-codex", "gpt-6.1-sol"),
    ("radius", "balanced"),
    ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-3.1-pro-preview"),
    ("google-vertex", "gemini-3.1-pro-preview"),
    ("github-copilot", "gpt-5.4"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("vercel-ai-gateway", "zai/glm-5.1"),
    ("xai", "grok-4.7"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "gpt-oss-120b"),
    ("zai", "glm-5.3"),
    ("zai-coding-cn", "glm-5.3"),
    ("mistral", "devstral-medium-latest"),
    ("minimax", "MiniMax-M2.7"),
    ("minimax-cn", "MiniMax-M2.7"),
    ("moonshotai", "kimi-k2.6"),
    ("moonshotai-cn", "kimi-k2.6"),
    ("huggingface", "moonshotai/Kimi-K2.6"),
    ("fireworks", "accounts/fireworks/models/kimi-k3"),
    ("together", "moonshotai/Kimi-K3"),
    ("baseten", "zai-org/GLM-5.2"),
    ("opencode", "kimi-k2.6"),
    ("opencode-go", "kimi-k3"),
    ("kimi-coding", "kimi-for-coding"),
    ("meta", "muse-spark-1.3"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
    (
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    ),
    ("qwen-token-plan", "qwen3.7-max"),
    ("qwen-token-plan-cn", "qwen3.7-max"),
    ("qwen-token-plan-individual", "qwen3.8-max"),
    ("xiaomi", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
];

/// pi's default thinking level.
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

fn default_model_id(provider: &str) -> Option<&'static str> {
    DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, model)| *model)
}

fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let bytes = id.as_bytes();
    !(bytes.len() >= 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit))
}

/// A model by exact reference: `provider/id`, or a bare id that is unique.
pub fn exact_match<'a>(reference: &str, models: &'a [Model]) -> Option<&'a Model> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    let lower = reference.to_lowercase();
    let unique = |matches: Vec<&'a Model>| (matches.len() == 1).then(|| matches[0]);
    let canonical: Vec<&Model> = models
        .iter()
        .filter(|model| model.reference().to_lowercase() == lower)
        .collect();
    if !canonical.is_empty() {
        return unique(canonical);
    }
    if let Some((provider, id)) = reference.split_once('/') {
        let (provider, id) = (provider.trim().to_lowercase(), id.trim().to_lowercase());
        if !provider.is_empty() && !id.is_empty() {
            let matches: Vec<&Model> = models
                .iter()
                .filter(|model| {
                    model.provider.to_lowercase() == provider && model.id.to_lowercase() == id
                })
                .collect();
            if !matches.is_empty() {
                return unique(matches);
            }
        }
    }
    unique(
        models
            .iter()
            .filter(|model| model.id.to_lowercase() == lower)
            .collect(),
    )
}

fn try_match<'a>(pattern: &str, models: &'a [Model]) -> Option<&'a Model> {
    if let Some(model) = exact_match(pattern, models) {
        return Some(model);
    }
    let lower = pattern.to_lowercase();
    let matches: Vec<&Model> = models
        .iter()
        .filter(|model| {
            model.id.to_lowercase().contains(&lower) || model.name.to_lowercase().contains(&lower)
        })
        .collect();
    let (mut aliases, mut dated): (Vec<&Model>, Vec<&Model>) =
        matches.into_iter().partition(|model| is_alias(&model.id));
    let pick = |list: &mut Vec<&'a Model>| {
        // pi sorts with `localeCompare`, which orders punctuation such as
        // OpenRouter's `~` before letters.
        list.sort_by(|a, b| yapi_types::collate::locale_compare(&b.id, &a.id));
        list.first().copied()
    };
    if aliases.is_empty() {
        pick(&mut dated)
    } else {
        pick(&mut aliases)
    }
}

/// A pattern match, with a thinking level from a `:level` suffix.
#[derive(Clone, Debug, Default)]
pub struct PatternMatch {
    /// The model.
    pub model: Option<Model>,
    /// The suffix level.
    pub thinking_level: Option<ThinkingLevel>,
    /// A problem worth reporting.
    pub warning: Option<String>,
}

/// Matches `pattern` (optionally `pattern:level`) against `models`.
pub fn parse_pattern(pattern: &str, models: &[Model], allow_invalid_level: bool) -> PatternMatch {
    if let Some(model) = try_match(pattern, models) {
        return PatternMatch {
            model: Some(model.clone()),
            ..PatternMatch::default()
        };
    }
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return PatternMatch::default();
    };
    match ThinkingLevel::parse(suffix) {
        Some(level) => {
            let result = parse_pattern(prefix, models, allow_invalid_level);
            if result.model.is_some() && result.warning.is_none() {
                PatternMatch {
                    thinking_level: Some(level),
                    ..result
                }
            } else {
                result
            }
        }
        None if !allow_invalid_level => PatternMatch::default(),
        None => {
            let result = parse_pattern(prefix, models, allow_invalid_level);
            match result.model {
                Some(model) => PatternMatch {
                    model: Some(model),
                    thinking_level: None,
                    warning: Some(format!(
                        "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
                    )),
                },
                None => result,
            }
        }
    }
}

/// A model in the session's scope, with the level its pattern named.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedModel {
    /// The model.
    pub model: Model,
    /// The level from a `:level` suffix; `None` keeps the session's level.
    pub thinking_level: Option<ThinkingLevel>,
}

/// pi's `resolveModelScopeFromModels`: the models `patterns` select from
/// `models`, in pattern order without duplicates, and a warning for each
/// pattern that selects nothing or names an invalid level. A pattern is a
/// glob over `provider/id` or the id, ignoring case, or a model pattern as
/// `--model` takes; either may end in `:level`.
pub fn resolve_model_scope(
    patterns: &[String],
    models: &[Model],
) -> (Vec<ScopedModel>, Vec<String>) {
    let mut scoped: Vec<ScopedModel> = Vec::new();
    let mut warnings = Vec::new();
    let mut add = |model: &Model, thinking_level: Option<ThinkingLevel>| {
        if !scoped
            .iter()
            .any(|entry| entry.model.provider == model.provider && entry.model.id == model.id)
        {
            scoped.push(ScopedModel {
                model: model.clone(),
                thinking_level,
            });
        }
    };
    for pattern in patterns {
        if pattern.contains(['*', '?', '[']) {
            let (glob, thinking_level) = match pattern.rsplit_once(':') {
                Some((glob, suffix)) => match ThinkingLevel::parse(suffix) {
                    Some(level) => (glob, Some(level)),
                    None => (pattern.as_str(), None),
                },
                None => (pattern.as_str(), None),
            };
            if let Some(model) = exact_match(glob, models) {
                add(model, thinking_level);
                continue;
            }
            let matching: Vec<&Model> = models
                .iter()
                .filter(|model| {
                    crate::glob::matches_with(glob, &model.reference(), true)
                        || crate::glob::matches_with(glob, &model.id, true)
                })
                .collect();
            if matching.is_empty() {
                warnings.push(format!("No models match pattern \"{pattern}\""));
            }
            for model in matching {
                add(model, thinking_level);
            }
            continue;
        }
        let found = parse_pattern(pattern, models, true);
        if let Some(warning) = found.warning {
            warnings.push(warning);
        }
        match &found.model {
            Some(model) => add(model, found.thinking_level),
            None => warnings.push(format!("No models match pattern \"{pattern}\"")),
        }
    }
    (scoped, warnings)
}

/// The result of `--model` resolution.
#[derive(Clone, Debug, Default)]
pub struct CliModel {
    /// The model.
    pub model: Option<Model>,
    /// A level from the pattern's suffix.
    pub thinking_level: Option<ThinkingLevel>,
    /// Shown on stderr.
    pub warning: Option<String>,
    /// Fatal.
    pub error: Option<String>,
}

/// Resolves `--provider` and `--model` against every model, authenticated or not.
pub fn resolve_cli_model(
    provider: Option<&str>,
    pattern: &str,
    cli_thinking: Option<ThinkingLevel>,
    registry: &ModelRegistry,
) -> CliModel {
    let models = registry.models();
    if models.is_empty() {
        return CliModel {
            error: Some(
                "No models available. Check your installation or add models to models.json.".into(),
            ),
            ..CliModel::default()
        };
    }
    let canonical = |name: &str| {
        models
            .iter()
            .find(|model| model.provider.eq_ignore_ascii_case(name))
            .map(|model| model.provider.clone())
    };
    let mut resolved_provider = match provider {
        Some(name) => match canonical(name) {
            Some(provider) => Some(provider),
            None => {
                return CliModel {
                    error: Some(format!(
                        "Unknown provider \"{name}\". Use --list-models to see available providers/models."
                    )),
                    ..CliModel::default()
                };
            }
        },
        None => None,
    };
    let mut pattern_text = pattern.to_owned();
    let mut inferred = false;
    if resolved_provider.is_none()
        && let Some((prefix, rest)) = pattern.split_once('/')
        && let Some(provider) = canonical(prefix)
    {
        resolved_provider = Some(provider);
        pattern_text = rest.to_owned();
        inferred = true;
    }
    if resolved_provider.is_none() {
        let lower = pattern.to_lowercase();
        let exact: Vec<&Model> = models
            .iter()
            .filter(|model| {
                model.id.to_lowercase() == lower || model.reference().to_lowercase() == lower
            })
            .collect();
        if exact.len() == 1 {
            return CliModel {
                model: Some(exact[0].clone()),
                ..CliModel::default()
            };
        }
        if exact.len() > 1 {
            let authenticated: Vec<&&Model> = exact
                .iter()
                .filter(|model| registry.has_auth(&model.provider))
                .collect();
            if authenticated.len() == 1 {
                return CliModel {
                    model: Some((*authenticated[0]).clone()),
                    ..CliModel::default()
                };
            }
            let mut names: Vec<String> = exact.iter().map(|model| model.reference()).collect();
            names.sort();
            let hint = if authenticated.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            return CliModel {
                error: Some(format!(
                    "Model \"{pattern}\" is ambiguous across providers: {}. {hint} Use --provider or provider/model.",
                    names.join(", ")
                )),
                ..CliModel::default()
            };
        }
    }
    if let (Some(_), Some(resolved)) = (provider, &resolved_provider) {
        let prefix = format!("{resolved}/");
        if pattern.to_lowercase().starts_with(&prefix.to_lowercase()) {
            pattern_text = pattern[prefix.len()..].to_owned();
        }
    }
    let candidates: Vec<Model> = match &resolved_provider {
        Some(provider) => models
            .iter()
            .filter(|m| &m.provider == provider)
            .cloned()
            .collect(),
        None => models.to_vec(),
    };
    let found = parse_pattern(&pattern_text, &candidates, false);
    if let Some(model) = found.model {
        if inferred && !registry.has_auth(&model.provider) {
            let others: Vec<&Model> = models
                .iter()
                .filter(|m| {
                    m.id.eq_ignore_ascii_case(pattern)
                        && !(m.provider == model.provider && m.id == model.id)
                })
                .filter(|m| registry.has_auth(&m.provider))
                .collect();
            if others.len() == 1 {
                return CliModel {
                    model: Some(others[0].clone()),
                    ..CliModel::default()
                };
            }
        }
        return CliModel {
            model: Some(model),
            thinking_level: found.thinking_level,
            warning: found.warning,
            error: None,
        };
    }
    if inferred {
        let lower = pattern.to_lowercase();
        if let Some(exact) = models
            .iter()
            .find(|m| m.id.to_lowercase() == lower || m.reference().to_lowercase() == lower)
        {
            return CliModel {
                model: Some(exact.clone()),
                ..CliModel::default()
            };
        }
        let fallback = parse_pattern(pattern, models, false);
        if fallback.model.is_some() {
            return CliModel {
                model: fallback.model,
                thinking_level: fallback.thinking_level,
                warning: fallback.warning,
                error: None,
            };
        }
    }
    if let Some(provider) = &resolved_provider {
        let mut fallback_pattern = pattern_text.clone();
        let mut fallback_thinking = None;
        if cli_thinking.is_none()
            && let Some((prefix, suffix)) = pattern_text.rsplit_once(':')
            && let Some(level) = ThinkingLevel::parse(suffix)
        {
            fallback_pattern = prefix.to_owned();
            fallback_thinking = Some(level);
        }
        let provider_models: Vec<&Model> =
            models.iter().filter(|m| &m.provider == provider).collect();
        if !provider_models.is_empty() {
            let base = default_model_id(provider)
                .and_then(|id| provider_models.iter().find(|m| m.id == id))
                .unwrap_or(&provider_models[0]);
            let mut model = Model {
                id: fallback_pattern.clone(),
                name: fallback_pattern.clone(),
                ..(*base).clone()
            };
            if cli_thinking
                .or(fallback_thinking)
                .is_some_and(|level| level != ThinkingLevel::Off)
            {
                model.reasoning = true;
            }
            let message = format!(
                "Model \"{fallback_pattern}\" not found for provider \"{provider}\". Using custom model id."
            );
            return CliModel {
                model: Some(model),
                thinking_level: fallback_thinking,
                warning: Some(match found.warning {
                    Some(warning) => format!("{warning} {message}"),
                    None => message,
                }),
                error: None,
            };
        }
    }
    let display = match &resolved_provider {
        Some(provider) => format!("{provider}/{pattern_text}"),
        None => pattern.to_owned(),
    };
    CliModel {
        warning: found.warning,
        error: Some(format!(
            "Model \"{display}\" not found. Use --list-models to see available models."
        )),
        ..CliModel::default()
    }
}

/// The starting model when `--model` is absent: the saved default if it has
/// credentials, else the first authenticated provider's default model, else the
/// first authenticated model.
pub fn initial_model(
    registry: &ModelRegistry,
    default_provider: Option<&str>,
    default_model: Option<&str>,
) -> Option<Model> {
    if let (Some(provider), Some(id)) = (default_provider, default_model)
        && let Some(model) = registry.find(provider, id)
        && registry.has_auth(provider)
    {
        return Some(model.clone());
    }
    let available = registry.available();
    for (provider, id) in DEFAULT_MODEL_PER_PROVIDER {
        if let Some(model) = available
            .iter()
            .find(|m| m.provider == *provider && m.id == *id)
        {
            return Some((*model).clone());
        }
    }
    available.first().map(|model| (*model).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_provider_prefixed_and_fuzzy_patterns() {
        let registry = ModelRegistry::builtin();
        let model = resolve_cli_model(None, "anthropic/claude-sonnet-4-5", None, &registry)
            .model
            .unwrap();
        assert_eq!(model.reference(), "anthropic/claude-sonnet-4-5");
        let result = resolve_cli_model(Some("anthropic"), "sonnet-4-5:high", None, &registry);
        assert_eq!(result.model.unwrap().provider, "anthropic");
        assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
        let fallback = resolve_cli_model(Some("anthropic"), "claude-unreleased", None, &registry);
        assert_eq!(fallback.model.unwrap().id, "claude-unreleased");
        assert!(fallback.warning.unwrap().contains("Using custom model id"));
        assert!(
            resolve_cli_model(Some("nope"), "x", None, &registry)
                .error
                .is_some()
        );
        assert!(is_alias("claude-sonnet-4-5"));
        assert!(!is_alias("claude-sonnet-4-5-20250929"));
    }

    #[test]
    fn fuzzy_matches_prefer_ids_in_locale_order() {
        let base = ModelRegistry::builtin().models()[0].clone();
        let model = |provider: &str, id: &str| Model {
            provider: provider.into(),
            id: id.into(),
            name: id.into(),
            ..base.clone()
        };
        // pi's `localeCompare` puts punctuation such as `~` before letters,
        // so the descending sort prefers `us.` over `~`.
        let models = [
            model("openrouter", "~anthropic/claude-sonnet-latest"),
            model("amazon-bedrock", "us.anthropic.claude-sonnet-5"),
        ];
        assert_eq!(
            try_match("sonnet", &models).map(|model| model.provider.as_str()),
            Some("amazon-bedrock")
        );
    }

    #[test]
    fn scopes_models_by_pattern_and_glob() {
        // As with credentials for Anthropic only.
        let registry = ModelRegistry::builtin();
        let models: Vec<Model> = registry
            .models()
            .iter()
            .filter(|model| model.provider == "anthropic")
            .cloned()
            .collect();
        let patterns: Vec<String> = [
            "claude-sonnet-4-5:high",
            "anthropic/claude-opus-4-*",
            "claude-sonnet-4-5",
            "zzz-nothing",
            "claude-haiku-4-5:bogus",
        ]
        .map(String::from)
        .to_vec();
        let (scoped, warnings) = resolve_model_scope(&patterns, &models);
        assert_eq!(scoped[0].model.reference(), "anthropic/claude-sonnet-4-5");
        assert_eq!(scoped[0].thinking_level, Some(ThinkingLevel::High));
        assert!(
            scoped[1..]
                .iter()
                .any(|s| s.model.id.starts_with("claude-opus-4-"))
        );
        // A repeated model keeps its first entry.
        assert_eq!(
            scoped
                .iter()
                .filter(|s| s.model.reference() == "anthropic/claude-sonnet-4-5")
                .count(),
            1
        );
        assert_eq!(
            scoped
                .last()
                .map(|s| (s.model.id.as_str(), s.thinking_level)),
            Some(("claude-haiku-4-5", None))
        );
        assert_eq!(
            warnings,
            [
                "No models match pattern \"zzz-nothing\"",
                "Invalid thinking level \"bogus\" in pattern \"claude-haiku-4-5:bogus\". Using default instead.",
            ]
        );
    }
}
