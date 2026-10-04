//! `ri auth`: credential printing and readiness checks for external clients.
//!
//! Port of pi's `auth-command.ts`, `auth-check.ts` and `credential-print.ts`.

use std::fmt::Write as _;
use std::io::Write as _;

use ri_ai::auth::CredentialKind;
use ri_ai::registry::{Auth, ModelRegistry};
use ri_core::model_resolver::resolve_cli_model;
use ri_types::auth::Credential;
use ri_types::model::Model;

/// A bearer token printed without `--min-expiry` stays valid this long.
const DEFAULT_BEARER_MIN_EXPIRY_MS: u64 = 30 * 60_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Check,
    ApiKey,
    BearerToken,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Check => "auth check",
            Kind::ApiKey => "auth print-api-key",
            Kind::BearerToken => "auth print-bearer-token",
        }
    }

    fn usage(self) -> &'static str {
        match self {
            Kind::Check => {
                "ri auth check --provider <provider> [--json] [--credentials] [--no-refresh]"
            }
            Kind::ApiKey => "ri auth print-api-key --provider <provider> [--model <model>]",
            Kind::BearerToken => {
                "ri auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
            }
        }
    }
}

struct Command {
    kind: Kind,
    args: Vec<String>,
    json: bool,
    credentials: bool,
    no_refresh: bool,
    min_expiry_ms: Option<u64>,
}

const HELP: &str = "Usage:
  ri auth print-api-key [--provider <provider>] [--model <model>]
  ri auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]
  ri auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.";

/// Runs `ri auth` with the arguments after the program name, `auth` first,
/// and returns the exit code: for checks 0 ready, 1 not ready, 2 invalid.
pub async fn run(args: &[String]) -> u8 {
    if args.get(1).is_none_or(|word| word == "help")
        || args.iter().any(|arg| arg == "--help" || arg == "-h")
    {
        let _ = writeln!(std::io::stdout(), "{HELP}");
        return 0;
    }
    let command = match parse(args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("Error: {message}");
            return 1;
        }
    };
    let parsed = crate::args::parse(&command.args);
    if let Some(option) = parsed.unknown_flags.keys().next() {
        eprintln!("Unknown option --{option} for \"{}\".", command.kind.name());
        eprintln!("Use \"ri --help\" or \"{}\".", command.kind.usage());
        return 1;
    }
    let failure = |message: String| {
        eprintln!("Error: {message}");
        if command.kind == Kind::Check { 2 } else { 1 }
    };
    if !parsed.diagnostics.is_empty() {
        let messages: Vec<&str> = parsed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        return failure(messages.join("\n"));
    }
    let (provider, model) = match validate(&parsed, command.kind) {
        Ok(requested) => requested,
        Err(message) => return failure(message),
    };
    let registry = ModelRegistry::load(&ri_core::config::agent_dir());
    if command.kind == Kind::Check {
        return check(&command, &registry, provider.as_deref(), model.as_deref()).await;
    }
    match credential_for_print(&command, &registry, provider.as_deref(), model.as_deref()).await {
        Ok(credential) => {
            let _ = writeln!(std::io::stdout(), "{credential}");
            0
        }
        Err(message) => failure(message),
    }
}

fn parse(args: &[String]) -> Result<Command, String> {
    let kind = match args.get(1).map(String::as_str) {
        Some("check") => Kind::Check,
        Some("print-api-key") => Kind::ApiKey,
        Some("print-bearer-token") => Kind::BearerToken,
        other => {
            return Err(format!(
                "Unknown auth command \"{}\". Use \"ri auth print-api-key\", \"ri auth print-bearer-token\", or \"ri auth check\".",
                other.unwrap_or_default()
            ));
        }
    };
    let mut command = Command {
        kind,
        args: Vec::new(),
        json: false,
        credentials: false,
        no_refresh: false,
        min_expiry_ms: None,
    };
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--min-expiry" => {
                if kind != Kind::BearerToken {
                    return Err("--min-expiry is only supported by print-bearer-token".into());
                }
                command.min_expiry_ms = Some(
                    rest.next()
                        .and_then(|value| duration_ms(value))
                        .ok_or("--min-expiry must use a duration such as 30m or 1h")?,
                );
            }
            "--json" | "--credentials" | "--no-refresh" => {
                if kind != Kind::Check {
                    return Err(format!("{arg} is only supported by auth check"));
                }
                match arg.as_str() {
                    "--json" => command.json = true,
                    "--credentials" => command.credentials = true,
                    _ => command.no_refresh = true,
                }
            }
            _ => command.args.push(arg.clone()),
        }
    }
    Ok(command)
}

/// pi's `/^(\d+)(ms|s|m|h)$/i` durations, in milliseconds.
fn duration_ms(value: &str) -> Option<u64> {
    let digits = value.chars().take_while(char::is_ascii_digit).count();
    let (amount, unit) = value.split_at(digits);
    let amount: u64 = amount.parse().ok()?;
    let scale = match unit.to_ascii_lowercase().as_str() {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    amount.checked_mul(scale)
}

/// The requested provider and model; pi's `validateAuthCommandArgs`.
fn validate(
    args: &crate::args::Args,
    kind: Kind,
) -> Result<(Option<String>, Option<String>), String> {
    let trimmed = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (provider, model) = (trimmed(&args.provider), trimmed(&args.model));
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("Auth commands only accept --provider and --model".into());
    }
    if provider.is_none() && model.is_none() {
        return Err(if kind == Kind::Check {
            "Auth checks require --provider <provider> or --model <model>".into()
        } else {
            "Credential printing requires --provider <provider> or --model <model>".into()
        });
    }
    Ok((provider, model))
}

/// pi's `getAuthCredential`: the API key, else a bearer token from the
/// `Authorization` header.
fn credential_of(auth: &Auth) -> Option<String> {
    if let Some(key) = auth.api_key.as_ref().filter(|key| !key.is_empty()) {
        return Some(key.clone());
    }
    auth.headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| value.as_deref())
        .and_then(|value| {
            let (scheme, token) = value.split_once(char::is_whitespace)?;
            let token = token.trim_start();
            (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_owned())
        })
}

/// A model of `provider`, for resolving its credentials.
fn provider_model<'a>(registry: &'a ModelRegistry, provider: &str) -> Option<&'a Model> {
    registry
        .models()
        .iter()
        .find(|model| model.provider == provider)
}

async fn check(
    command: &Command,
    registry: &ModelRegistry,
    provider: Option<&str>,
    model: Option<&str>,
) -> u8 {
    // As in pi, a model that does not resolve makes the check invalid rather
    // than an error, labeled with what was asked for.
    let requested = provider.or(model).unwrap_or_default().to_owned();
    let mut provider = provider.map(str::to_owned);
    let mut unresolved = false;
    if let Some(pattern) = model {
        let resolved = resolve_cli_model(provider.as_deref(), pattern, None, registry);
        match (resolved.error, resolved.model) {
            (None, Some(model)) => provider = Some(model.provider),
            _ => unresolved = true,
        }
    }
    let provider = match provider {
        Some(provider) if !unresolved => provider,
        _ => requested,
    };
    let mut credential = None;
    let (status, reason, auth_type) = if unresolved || registry.error().is_some() {
        ("invalid", Some("invalid_state"), None)
    } else if let Some(model) = provider_model(registry, &provider) {
        if !registry.has_auth(&provider) {
            ("not_ready", Some("credentials_not_configured"), None)
        } else {
            let oauth = registry.is_using_oauth(&provider);
            let auth_type = Some(if oauth { "oauth" } else { "api_key" });
            let auth = if command.no_refresh {
                None
            } else {
                Some(registry.auth(model).await)
            };
            // A failed OAuth refresh is an invalid state, as pi's thrown error.
            if auth.as_ref().is_some_and(|auth| auth.error.is_some()) {
                ("invalid", Some("invalid_state"), None)
            } else if auth
                .as_ref()
                .is_some_and(|auth| credential_of(auth).is_none())
            {
                ("not_ready", Some("credentials_not_configured"), None)
            } else if command.credentials {
                // Without refreshing, a stored OAuth token is printed as stored.
                credential = match (&auth, registry.store().get(&provider)) {
                    (None, Some(Credential::OAuth(stored))) => Some(stored.access),
                    (Some(auth), _) => credential_of(auth),
                    (None, _) => credential_of(&registry.auth(model).await),
                };
                match credential {
                    Some(_) => ("ready", None, auth_type),
                    None => ("not_ready", Some("credential_not_available"), None),
                }
            } else {
                ("ready", None, auth_type)
            }
        }
    } else {
        ("not_ready", Some("provider_not_found"), None)
    };
    let output = if command.json {
        let mut object = serde_json::Map::new();
        object.insert("status".into(), status.into());
        object.insert("provider".into(), provider.as_str().into());
        if let Some(reason) = reason {
            object.insert("reason".into(), reason.into());
        }
        if let Some(auth_type) = auth_type {
            object.insert("authType".into(), auth_type.into());
        }
        if let Some(credential) = &credential {
            object.insert("credentials".into(), credential.as_str().into());
        }
        ri_types::json::to_string(&serde_json::Value::Object(object)).unwrap_or_default()
    } else {
        credential.unwrap_or_else(|| status.to_owned())
    };
    let _ = writeln!(std::io::stdout(), "{output}");
    match status {
        "ready" => 0,
        "not_ready" => 1,
        _ => 2,
    }
}

/// pi's `resolveCredentialForPrint`: the one configured credential of the
/// requested kind.
async fn credential_for_print(
    command: &Command,
    registry: &ModelRegistry,
    provider: Option<&str>,
    pattern: Option<&str>,
) -> Result<String, String> {
    let stored = registry.stored_credentials();
    let stored_kind = |id: &str| {
        stored
            .iter()
            .find(|(provider, _)| provider == id)
            .map(|(_, kind)| *kind)
    };
    let mut candidates: Vec<(String, Model)> = Vec::new();
    match provider {
        Some(name) => {
            let Some(model) = provider_model(registry, name) else {
                return Err(format!(
                    "Unknown provider \"{name}\". Use --list-models to see available providers."
                ));
            };
            let model = match pattern {
                Some(pattern) => {
                    let resolved = resolve_cli_model(Some(name), pattern, None, registry);
                    match (resolved.error, resolved.model) {
                        (None, Some(model)) => model,
                        (error, _) => {
                            return Err(error.unwrap_or_else(|| {
                                "Unable to resolve the requested provider/model".into()
                            }));
                        }
                    }
                }
                None => model.clone(),
            };
            candidates.push((model.provider.clone(), model));
        }
        None => {
            let pattern = pattern.unwrap_or_default();
            let mut providers: Vec<&str> = Vec::new();
            for model in registry.models() {
                if !providers.contains(&model.provider.as_str()) {
                    providers.push(&model.provider);
                }
            }
            for id in providers {
                if stored_kind(id).is_none() {
                    continue;
                }
                let resolved = resolve_cli_model(Some(id), pattern, None, registry);
                if let (None, Some(model)) = (&resolved.error, resolved.model)
                    && !resolved
                        .warning
                        .is_some_and(|warning| warning.contains("Using custom model id"))
                {
                    candidates.push((id.to_owned(), model));
                }
            }
            if candidates.is_empty() {
                return Err(format!(
                    "Model \"{pattern}\" not found. Use --list-models to see available models."
                ));
            }
        }
    }
    let mut found: Vec<(String, String)> = Vec::new();
    for (id, model) in &candidates {
        let kind = stored_kind(id);
        let oauth = kind == Some(CredentialKind::OAuth);
        if (command.kind == Kind::ApiKey && oauth) || (command.kind == Kind::BearerToken && !oauth)
        {
            continue;
        }
        let auth = if command.kind == Kind::BearerToken {
            let min = command
                .min_expiry_ms
                .unwrap_or(DEFAULT_BEARER_MIN_EXPIRY_MS);
            registry.auth_valid_for(model, min).await
        } else {
            registry.auth(model).await
        };
        if let Some(value) = credential_of(&auth) {
            found.push((id.clone(), value));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0).1),
        0 => {
            let first = candidates.first().map(|(id, _)| id.as_str());
            let oauth = first.and_then(stored_kind) == Some(CredentialKind::OAuth);
            if let (Some(_), Some(id)) = (provider, first) {
                if command.kind == Kind::ApiKey && oauth {
                    return Err(format!(
                        "Provider \"{id}\" is configured with OAuth, not an API key"
                    ));
                }
                if command.kind == Kind::BearerToken && !oauth {
                    return Err(format!(
                        "Provider \"{id}\" is not configured with an OAuth bearer token"
                    ));
                }
            }
            Err(format!(
                "No usable {} is configured",
                if command.kind == Kind::ApiKey {
                    "API key"
                } else {
                    "OAuth bearer token"
                }
            ))
        }
        _ => {
            let mut names = String::new();
            for (index, (id, _)) in found.iter().enumerate() {
                let _ = write!(names, "{}{id}", if index == 0 { "" } else { ", " });
            }
            Err(format!(
                "Multiple configured providers matched ({names}). Specify --provider."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn parses_pi_auth_commands() {
        assert!(parse(&args(&["auth", "login"])).is_err());
        let command = parse(&args(&["auth", "check", "--provider", "x", "--json"])).unwrap();
        assert!(command.kind == Kind::Check && command.json);
        assert_eq!(command.args, ["--provider", "x"]);
        assert_eq!(
            parse(&args(&["auth", "print-api-key", "--json"]))
                .err()
                .as_deref(),
            Some("--json is only supported by auth check")
        );
        let command = parse(&args(&["auth", "print-bearer-token", "--min-expiry", "2h"])).unwrap();
        assert_eq!(command.min_expiry_ms, Some(7_200_000));
        assert!(parse(&args(&["auth", "print-bearer-token", "--min-expiry", "2d"])).is_err());
        assert_eq!(duration_ms("250MS"), Some(250));
        assert_eq!(duration_ms("m"), None);
    }

    #[test]
    fn bearer_tokens_come_from_the_authorization_header() {
        let mut auth = Auth::default();
        assert_eq!(credential_of(&auth), None);
        auth.headers
            .insert("authorization".into(), Some("Bearer  abc".into()));
        assert_eq!(credential_of(&auth).as_deref(), Some("abc"));
        auth.api_key = Some("key".into());
        assert_eq!(credential_of(&auth).as_deref(), Some("key"));
    }
}
