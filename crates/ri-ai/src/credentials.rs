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

/// The variables the value reads that are not set.
pub fn missing_env_vars(config: &str, env: Option<&ProviderEnv>) -> Vec<String> {
    env_var_names(config)
        .into_iter()
        .filter(|name| env_value(name, env).is_none())
        .collect()
}

/// pi's `resolveConfigValueOrThrow` message for a value that did not
/// resolve, describing it as `description`.
pub fn unresolved_message(config: &str, description: &str, env: Option<&ProviderEnv>) -> String {
    if let Some(command) = config.strip_prefix('!') {
        return format!("Failed to resolve {description} from shell command: {command}");
    }
    match missing_env_vars(config, env).as_slice() {
        [] => format!("Failed to resolve {description}"),
        [name] => format!("Failed to resolve {description} from environment variable: {name}"),
        names => format!(
            "Failed to resolve {description} from environment variables: {}",
            names.join(", ")
        ),
    }
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
    crate::providers::info(provider)
        .and_then(|info| info.api_key)
        .map_or(&[], |method| method.env)
}

/// The variable Anthropic reads as a bearer token rather than an API key.
pub const BEARER_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

/// The key pi reports for providers authenticated by ambient credentials.
pub const AMBIENT_CREDENTIALS: &str = "<authenticated>";

/// Whether Google Application Default Credentials exist: the file named by
/// `GOOGLE_APPLICATION_CREDENTIALS`, or gcloud's default file.
fn has_vertex_adc_credentials(env: Option<&ProviderEnv>) -> bool {
    match env_value("GOOGLE_APPLICATION_CREDENTIALS", env) {
        Some(path) => std::path::Path::new(&path).exists(),
        None => std::env::var_os("HOME").is_some_and(|home| {
            std::path::Path::new(&home)
                .join(".config/gcloud/application_default_credentials.json")
                .exists()
        }),
    }
}

/// Ambient credentials that authenticate a provider without an API key: AWS
/// profiles, keys, tokens and roles for Bedrock; ADC with a project and location
/// for Vertex. Returns the variable that makes it so.
fn ambient_credentials(provider: &str, env: Option<&ProviderEnv>) -> Option<&'static str> {
    let set = |name: &str| env_value(name, env).is_some();
    match provider {
        "google-vertex" => (has_vertex_adc_credentials(env)
            && (set("GOOGLE_CLOUD_PROJECT") || set("GCLOUD_PROJECT"))
            && set("GOOGLE_CLOUD_LOCATION"))
        .then_some("GOOGLE_CLOUD_LOCATION"),
        "amazon-bedrock" => {
            if set("AWS_PROFILE") {
                Some("AWS_PROFILE")
            } else if set("AWS_ACCESS_KEY_ID") && set("AWS_SECRET_ACCESS_KEY") {
                Some("AWS_ACCESS_KEY_ID")
            } else {
                [
                    "AWS_BEARER_TOKEN_BEDROCK",
                    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                    "AWS_WEB_IDENTITY_TOKEN_FILE",
                ]
                .into_iter()
                .find(|name| set(name))
            }
        }
        _ => None,
    }
}

/// The first set environment variable for a provider's key, with its name. For
/// providers with ambient credentials (Bedrock, Vertex) the key is
/// [`AMBIENT_CREDENTIALS`].
pub fn env_api_key(provider: &str, env: Option<&ProviderEnv>) -> Option<(&'static str, String)> {
    api_key_env_vars(provider)
        .iter()
        .find_map(|name| env_value(name, env).map(|value| (*name, value)))
        .or_else(|| {
            ambient_credentials(provider, env).map(|name| (name, AMBIENT_CREDENTIALS.to_owned()))
        })
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
        let mut aws = ProviderEnv::new();
        aws.insert("AWS_ACCESS_KEY_ID".into(), "id".into());
        aws.insert("AWS_SECRET_ACCESS_KEY".into(), "secret".into());
        assert_eq!(
            env_api_key("amazon-bedrock", Some(&aws)).map(|(_, key)| key),
            Some(AMBIENT_CREDENTIALS.to_owned())
        );
    }
}
