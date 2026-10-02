//! Credential values: `$VAR` and `${VAR}` templates, `!command` output, and the
//! environment variables each provider reads.
//!
//! Ports of `resolve-config-value.ts` and `env-api-keys.ts` in pi `v1.0.0`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use indexmap::IndexMap;

/// Extra variables that take priority over the process environment.
pub type ProviderEnv = IndexMap<String, String>;

enum Part {
    Literal(String),
    Env(String),
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn push_literal(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Part::Literal(previous)) = parts.last_mut() {
        previous.push_str(text);
    } else {
        parts.push(Part::Literal(text.to_owned()));
    }
}

fn parse_template(config: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut rest = config;
    while let Some(dollar) = rest.find('$') {
        push_literal(&mut parts, &rest[..dollar]);
        let after = &rest[dollar + 1..];
        match after.chars().next() {
            Some(c @ ('$' | '!')) => {
                push_literal(&mut parts, &c.to_string());
                rest = &after[1..];
            }
            Some('{') => match after.find('}') {
                None => {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
                Some(end) => {
                    let name = &after[1..end];
                    let valid =
                        name.chars().next().is_some_and(is_name_start) && name.chars().all(is_name);
                    if valid {
                        parts.push(Part::Env(name.to_owned()));
                    } else {
                        push_literal(&mut parts, &rest[dollar..dollar + end + 2]);
                    }
                    rest = &after[end + 1..];
                }
            },
            Some(c) if is_name_start(c) => {
                let length = after.find(|c: char| !is_name(c)).unwrap_or(after.len());
                parts.push(Part::Env(after[..length].to_owned()));
                rest = &after[length..];
            }
            _ => {
                push_literal(&mut parts, "$");
                rest = after;
            }
        }
    }
    push_literal(&mut parts, rest);
    parts
}

fn env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    env.and_then(|env| env.get(name))
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// Whether the value runs a command.
pub fn is_command(config: &str) -> bool {
    config.starts_with('!')
}

/// The environment variables a template value reads.
pub fn env_var_names(config: &str) -> Vec<String> {
    if is_command(config) {
        return Vec::new();
    }
    let mut names = Vec::new();
    for part in parse_template(config) {
        if let Part::Env(name) = part
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// Whether every variable the value reads is set.
pub fn is_configured(config: &str, env: Option<&ProviderEnv>) -> bool {
    env_var_names(config)
        .iter()
        .all(|name| env_value(name, env).is_some())
}

/// Resolves a value: `!command` runs through the shell (cached when `cache`),
/// anything else is a template whose variables must all be set.
pub async fn resolve(config: &str, env: Option<&ProviderEnv>, cache: bool) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return run_command(command, cache).await;
    }
    let mut resolved = String::new();
    for part in parse_template(config) {
        match part {
            Part::Literal(text) => resolved.push_str(&text),
            Part::Env(name) => resolved.push_str(&env_value(&name, env)?),
        }
    }
    Some(resolved)
}

async fn run_command(command: &str, cache: bool) -> Option<String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    let cache_map = CACHE.get_or_init(Default::default);
    if cache
        && let Some(value) = cache_map
            .lock()
            .ok()
            .and_then(|map| map.get(command).cloned())
    {
        return value;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await;
    let value = match output {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    };
    if cache && let Ok(mut map) = cache_map.lock() {
        map.insert(command.to_owned(), value.clone());
    }
    value
}

/// The environment variables holding a provider's API key, in priority order.
/// Anthropic's `ANTHROPIC_AUTH_TOKEN` is a bearer token, not a key; see
/// [`BEARER_TOKEN_ENV`].
pub fn api_key_env_vars(provider: &str) -> &'static [&'static str] {
    match provider {
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "anthropic" => &[
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "qwen-token-plan" | "qwen-token-plan-individual" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "typesafe" => &["TYPESAFE_API_KEY"],
        "radius" => &["RADIUS_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "baseten" => &["BASETEN_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "meta" => &["META_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        _ => &[],
    }
}

/// The variable Anthropic reads as a bearer token rather than an API key.
pub const BEARER_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

/// The first set environment variable for a provider's key, with its name.
pub fn env_api_key(provider: &str, env: Option<&ProviderEnv>) -> Option<(&'static str, String)> {
    api_key_env_vars(provider)
        .iter()
        .find_map(|name| env_value(name, env).map(|value| (*name, value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_templates_and_commands() {
        let mut env = ProviderEnv::new();
        env.insert("RI_TEST_A".into(), "a".into());
        assert_eq!(
            resolve("x-$RI_TEST_A-${RI_TEST_A}$$!", Some(&env), false)
                .await
                .as_deref(),
            Some("x-a-a$!")
        );
        assert_eq!(
            resolve("$RI_TEST_UNSET_VALUE", Some(&env), false).await,
            None
        );
        assert_eq!(
            resolve("${not valid}", None, false).await.as_deref(),
            Some("${not valid}")
        );
        assert_eq!(resolve("!echo hi", None, true).await.as_deref(), Some("hi"));
        assert_eq!(resolve("!exit 1", None, false).await, None);
        assert_eq!(env_var_names("$A and ${B} $A"), ["A", "B"]);
        assert!(!is_configured("$RI_TEST_UNSET_VALUE", None));
    }
}
