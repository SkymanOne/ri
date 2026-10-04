//! API-key credentials of the providers whose setup is more than one key in
//! one environment variable: Amazon Bedrock, Google Vertex AI, the two
//! Cloudflare providers, llama.cpp, and Anthropic's workload identity
//! federation.
//!
//! Ports of the `ApiKeyAuth` objects in `providers/amazon-bedrock.ts`,
//! `google-vertex.ts`, `cloudflare-auth.ts` and `anthropic.ts` in pi-ai, and
//! of `extensions/llama/provider.ts` in pi-coding-agent, `v1.0.0`.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use ri_types::auth::ApiKeyCredential;

use crate::auth::{AuthError, AuthEvent, AuthPrompt, Interaction, Link, SelectOption};
use crate::credentials::ProviderEnv;

/// Request credentials from a stored credential or the environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Sent as the API key.
    pub api_key: Option<String>,
    /// Headers that carry the credential; `None` removes a header.
    pub headers: IndexMap<String, Option<String>>,
    /// Provider settings the request reads, such as a Cloudflare account id.
    pub env: Option<ProviderEnv>,
    /// Where the credential came from, as pi labels it.
    pub source: String,
    /// Replaces the model's base URL, as a llama.cpp server URL does.
    pub base_url: Option<String>,
}

/// What resolution reads besides the stored credential.
pub struct Ambient<'a> {
    /// An environment variable; blank values count as unset.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Whether a file exists; a leading `~` is the home directory.
    pub file_exists: &'a dyn Fn(&str) -> bool,
}

impl Ambient<'static> {
    /// The process environment and file system.
    pub fn process() -> Ambient<'static> {
        Ambient {
            env: &process_env,
            file_exists: &file_exists,
        }
    }
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn file_exists(path: &str) -> bool {
    let path = match path.strip_prefix('~') {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(format!("{}{rest}", home.to_string_lossy())),
            None => return false,
        },
        None => PathBuf::from(path),
    };
    Path::new(&path).exists()
}

/// Whether `provider`'s key auth is resolved here rather than from its key
/// variables.
pub fn is_custom(provider: &str) -> bool {
    matches!(
        provider,
        "amazon-bedrock"
            | "google-vertex"
            | "cloudflare-workers-ai"
            | "cloudflare-ai-gateway"
            | crate::llama::PROVIDER_ID
    )
}

/// pi's `resolve` for a custom provider: from `credential`, whose key is
/// already resolved, else the environment. `None` when the provider is not
/// configured or is not custom.
pub fn resolve(
    provider: &str,
    credential: Option<&ApiKeyCredential>,
    ambient: &Ambient<'_>,
) -> Option<Resolved> {
    match provider {
        "amazon-bedrock" => bedrock(credential, ambient),
        "google-vertex" => vertex(credential, ambient),
        "cloudflare-workers-ai" => cloudflare(false, credential, ambient),
        "cloudflare-ai-gateway" => cloudflare(true, credential, ambient),
        crate::llama::PROVIDER_ID => llama(credential, ambient),
        _ => None,
    }
}

/// llama.cpp: the server URL from the credential, else `LLAMA_BASE_URL`;
/// the key from the credential, else `LLAMA_API_KEY`, else `local`.
fn llama(credential: Option<&ApiKeyCredential>, ambient: &Ambient<'_>) -> Option<Resolved> {
    use crate::llama::{BASE_URL_ENV, inference_url, normalize_server_url};
    let configured = stored_env(credential, BASE_URL_ENV)
        .filter(|url| !url.trim().is_empty())
        .or_else(|| (ambient.env)(BASE_URL_ENV))?;
    let server = normalize_server_url(&configured).ok()?;
    let api_key = credential
        .and_then(|credential| credential.key.clone())
        .or_else(|| (ambient.env)("LLAMA_API_KEY"))
        .unwrap_or_else(|| "local".into());
    let mut env = credential
        .and_then(|credential| credential.env.clone())
        .unwrap_or_default();
    env.insert(BASE_URL_ENV.into(), server.clone());
    Some(Resolved {
        api_key: Some(api_key),
        headers: IndexMap::new(),
        env: Some(env),
        source: if credential.is_some() {
            "stored credential"
        } else {
            BASE_URL_ENV
        }
        .into(),
        base_url: Some(inference_url(&server)),
    })
}

fn stored_key(credential: Option<&ApiKeyCredential>) -> Option<&str> {
    credential
        .and_then(|credential| credential.key.as_deref())
        .filter(|key| !key.is_empty())
}

fn stored_env(credential: Option<&ApiKeyCredential>, name: &str) -> Option<String> {
    credential
        .and_then(|credential| credential.env.as_ref())
        .and_then(|env| env.get(name))
        .cloned()
}

fn bedrock(credential: Option<&ApiKeyCredential>, ambient: &Ambient<'_>) -> Option<Resolved> {
    let env = |name: &str| (ambient.env)(name);
    let resolved = |source: &str, env: Option<ProviderEnv>| Resolved {
        source: source.to_owned(),
        env,
        ..Resolved::default()
    };
    if let Some(key) = stored_key(credential) {
        return Some(Resolved {
            api_key: Some(key.to_owned()),
            ..resolved("stored credential", credential.and_then(|c| c.env.clone()))
        });
    }
    if env("AWS_BEARER_TOKEN_BEDROCK").is_some() {
        return Some(resolved("AWS_BEARER_TOKEN_BEDROCK", None));
    }
    let stored_profile = stored_env(credential, "AWS_PROFILE").filter(|p| !p.is_empty());
    if stored_profile.is_some() || env("AWS_PROFILE").is_some() {
        let source = if stored_profile.is_some() {
            "stored credential"
        } else {
            "AWS_PROFILE"
        };
        return Some(resolved(source, credential.and_then(|c| c.env.clone())));
    }
    if env("AWS_ACCESS_KEY_ID").is_some() && env("AWS_SECRET_ACCESS_KEY").is_some() {
        return Some(resolved("AWS access keys", None));
    }
    if env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || env("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        return Some(resolved("ECS task role", None));
    }
    if env("AWS_WEB_IDENTITY_TOKEN_FILE").is_some() {
        return Some(resolved("web identity token", None));
    }
    None
}

/// gcloud's Application Default Credentials file.
const VERTEX_ADC_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

fn vertex(credential: Option<&ApiKeyCredential>, ambient: &Ambient<'_>) -> Option<Resolved> {
    let env = |name: &str| (ambient.env)(name);
    // A stored key, even an empty one, stands in for the variable.
    let (key, source) = match credential.and_then(|c| c.key.clone()) {
        Some(key) => (key, "stored credential"),
        None => (
            env("GOOGLE_CLOUD_API_KEY").unwrap_or_default(),
            "GOOGLE_CLOUD_API_KEY",
        ),
    };
    if !key.is_empty() {
        return Some(Resolved {
            api_key: Some(key),
            source: source.into(),
            ..Resolved::default()
        });
    }
    let adc = stored_env(credential, "GOOGLE_APPLICATION_CREDENTIALS")
        .or_else(|| env("GOOGLE_APPLICATION_CREDENTIALS"));
    let has_credentials = (ambient.file_exists)(adc.as_deref().unwrap_or(VERTEX_ADC_PATH));
    let project = stored_env(credential, "GOOGLE_CLOUD_PROJECT")
        .or_else(|| env("GOOGLE_CLOUD_PROJECT"))
        .or_else(|| env("GCLOUD_PROJECT"));
    let location =
        stored_env(credential, "GOOGLE_CLOUD_LOCATION").or_else(|| env("GOOGLE_CLOUD_LOCATION"));
    let configured = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.is_empty());
    (has_credentials && configured(&project) && configured(&location)).then(|| Resolved {
        env: credential.and_then(|c| c.env.clone()),
        source: if credential.is_some() {
            "stored credential"
        } else {
            "gcloud application default credentials"
        }
        .into(),
        ..Resolved::default()
    })
}

const CLOUDFLARE_API_KEY: &str = "CLOUDFLARE_API_KEY";
const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

fn cloudflare(
    gateway: bool,
    credential: Option<&ApiKeyCredential>,
    ambient: &Ambient<'_>,
) -> Option<Resolved> {
    // Per field: the credential's value, else the environment, so a stored
    // key still picks up the account and gateway ids from the environment.
    let value = |name: &str| {
        let stored = credential.and_then(|credential| {
            if name == CLOUDFLARE_API_KEY {
                credential.key.clone()
            } else {
                credential.env.as_ref()?.get(name).cloned()
            }
        });
        stored.or_else(|| (ambient.env)(name))
    };
    let api_key = value(CLOUDFLARE_API_KEY).filter(|v| !v.is_empty())?;
    let account = value(CLOUDFLARE_ACCOUNT_ID).filter(|v| !v.is_empty())?;
    let mut env = ProviderEnv::new();
    env.insert(CLOUDFLARE_ACCOUNT_ID.into(), account);
    let source = if credential.is_some() {
        "stored credential"
    } else {
        CLOUDFLARE_API_KEY
    }
    .to_owned();
    if !gateway {
        return Some(Resolved {
            api_key: Some(api_key),
            headers: IndexMap::new(),
            env: Some(env),
            source,
            base_url: None,
        });
    }
    let gateway_id = value(CLOUDFLARE_GATEWAY_ID).filter(|v| !v.is_empty())?;
    env.insert(CLOUDFLARE_GATEWAY_ID.into(), gateway_id);
    let mut headers = IndexMap::new();
    headers.insert(
        "cf-aig-authorization".to_owned(),
        Some(format!("Bearer {api_key}")),
    );
    headers.insert("Authorization".to_owned(), None);
    headers.insert("x-api-key".to_owned(), None);
    Some(Resolved {
        api_key: None,
        headers,
        env: Some(env),
        source,
        base_url: None,
    })
}

/// The variables of Anthropic's workload identity federation: the three it
/// needs, then the two passed through when set.
const FEDERATION_REQUIRED: [&str; 3] = [
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
    "ANTHROPIC_IDENTITY_TOKEN_FILE",
];
const FEDERATION_OPTIONAL: [&str; 2] = ["ANTHROPIC_SERVICE_ACCOUNT_ID", "ANTHROPIC_WORKSPACE_ID"];

/// Anthropic's last resort when no key or token is set: workload identity
/// federation, whose ids travel to the request as provider settings.
pub fn anthropic_federation(ambient: &Ambient<'_>) -> Option<Resolved> {
    let mut env = ProviderEnv::new();
    for name in FEDERATION_REQUIRED {
        env.insert(name.to_owned(), (ambient.env)(name)?);
    }
    for name in FEDERATION_OPTIONAL {
        if let Some(value) = (ambient.env)(name) {
            env.insert(name.to_owned(), value);
        }
    }
    Some(Resolved {
        env: Some(env),
        source: "workload identity federation".into(),
        ..Resolved::default()
    })
}

fn select(message: &str, options: &[(&str, &str)]) -> AuthPrompt {
    AuthPrompt::Select {
        message: message.to_owned(),
        options: options
            .iter()
            .map(|(id, label)| SelectOption {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
            })
            .collect(),
    }
}

fn text(message: &str) -> AuthPrompt {
    AuthPrompt::Text {
        message: message.to_owned(),
        placeholder: None,
    }
}

fn secret(message: &str) -> AuthPrompt {
    AuthPrompt::Secret {
        message: message.to_owned(),
    }
}

fn info(interaction: &Interaction, message: &str, label: &str, url: &str) {
    interaction.notify(AuthEvent::Info {
        message: message.to_owned(),
        links: vec![Link {
            label: label.to_owned(),
            url: url.to_owned(),
        }],
    });
}

fn env_of(pairs: &[(&str, String)]) -> Option<ProviderEnv> {
    Some(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect(),
    )
}

/// pi's `login` for a custom provider: the questions that build its stored
/// credential. `None` when the provider is not custom.
pub async fn login(
    provider: &str,
    interaction: &Interaction,
) -> Option<Result<ApiKeyCredential, AuthError>> {
    Some(match provider {
        "amazon-bedrock" => bedrock_login(interaction).await,
        "google-vertex" => vertex_login(interaction).await,
        "cloudflare-workers-ai" => cloudflare_login(false, interaction).await,
        "cloudflare-ai-gateway" => cloudflare_login(true, interaction).await,
        crate::llama::PROVIDER_ID => llama_login(interaction).await,
        _ => return None,
    })
}

/// pi's llama.cpp sign-in: the server URL and an optional key, checked by
/// listing the server's models.
async fn llama_login(interaction: &Interaction) -> Result<ApiKeyCredential, AuthError> {
    use crate::llama::{BASE_URL_ENV, Client, DEFAULT_SERVER_URL, normalize_server_url};
    interaction.check()?;
    let fallback = process_env(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_SERVER_URL.into());
    let entered = interaction
        .prompt(AuthPrompt::Text {
            message: "llama.cpp server URL".into(),
            placeholder: Some(fallback.clone()),
        })
        .await?;
    let entered = entered.trim();
    let server = normalize_server_url(if entered.is_empty() {
        &fallback
    } else {
        entered
    })
    .map_err(AuthError::Failed)?;
    interaction.check()?;
    let key = interaction
        .prompt(secret("API key (optional)"))
        .await?
        .trim()
        .to_owned();
    let key = (!key.is_empty()).then_some(key);
    Client::new(&server, key.clone())
        .map_err(AuthError::Failed)?
        .list(false, interaction.cancel())
        .await
        .map_err(AuthError::Failed)?;
    Ok(ApiKeyCredential {
        key,
        env: env_of(&[(BASE_URL_ENV, server)]),
    })
}

async fn bedrock_login(interaction: &Interaction) -> Result<ApiKeyCredential, AuthError> {
    interaction.check()?;
    let method = interaction
        .prompt(select(
            "Select Amazon Bedrock authentication method:",
            &[
                ("bearer-token", "Bearer token"),
                ("aws-profile", "AWS profile"),
                ("credential-chain", "Existing AWS credential chain"),
            ],
        ))
        .await?;
    interaction.check()?;
    if method == "bearer-token" {
        let key = interaction
            .prompt(secret("Enter Amazon Bedrock bearer token"))
            .await?;
        return Ok(ApiKeyCredential {
            key: Some(key),
            env: None,
        });
    }
    info(
        interaction,
        "Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials.",
        "AWS credential provider chain",
        "https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html",
    );
    match method.as_str() {
        "aws-profile" => {
            let profile = interaction.prompt(text("Enter AWS profile name")).await?;
            Ok(ApiKeyCredential {
                key: None,
                env: env_of(&[("AWS_PROFILE", profile)]),
            })
        }
        "credential-chain" => {
            interaction
                .prompt(text(
                    "Configure AWS credentials, then press Enter to continue",
                ))
                .await?;
            Ok(ApiKeyCredential {
                key: None,
                env: None,
            })
        }
        other => Err(AuthError::failed(format!(
            "Unknown Amazon Bedrock auth method: {other}"
        ))),
    }
}

async fn vertex_login(interaction: &Interaction) -> Result<ApiKeyCredential, AuthError> {
    interaction.check()?;
    let method = interaction
        .prompt(select(
            "Select Google Vertex AI authentication method:",
            &[
                ("api-key", "Google Cloud API key"),
                ("adc", "Application Default Credentials"),
                ("service-account", "Service account credentials file"),
            ],
        ))
        .await?;
    interaction.check()?;
    if method == "api-key" {
        let key = interaction
            .prompt(secret("Enter Google Cloud API key"))
            .await?;
        return Ok(ApiKeyCredential {
            key: Some(key),
            env: None,
        });
    }
    if method != "adc" && method != "service-account" {
        return Err(AuthError::failed(format!(
            "Unknown Google Vertex AI auth method: {method}"
        )));
    }
    let message = if method == "adc" {
        "Run `gcloud auth application-default login`, then provide the project and location."
    } else {
        "Provide a service account credentials file, project, and location."
    };
    info(
        interaction,
        message,
        "Application Default Credentials",
        "https://cloud.google.com/docs/authentication/provide-credentials-adc",
    );
    let project = interaction
        .prompt(text("Enter Google Cloud project ID"))
        .await?;
    let location = interaction
        .prompt(text("Enter Google Cloud location"))
        .await?;
    let mut env = vec![
        ("GOOGLE_CLOUD_PROJECT", project),
        ("GOOGLE_CLOUD_LOCATION", location),
    ];
    if method == "service-account" {
        let path = interaction
            .prompt(text("Enter service account credentials file path"))
            .await?;
        // pi spreads the path only when it is non-empty.
        if !path.is_empty() {
            env.push(("GOOGLE_APPLICATION_CREDENTIALS", path));
        }
    }
    Ok(ApiKeyCredential {
        key: None,
        env: env_of(&env),
    })
}

async fn cloudflare_login(
    gateway: bool,
    interaction: &Interaction,
) -> Result<ApiKeyCredential, AuthError> {
    let key = interaction
        .prompt(secret("Enter Cloudflare API key"))
        .await?;
    let account = interaction
        .prompt(text("Enter Cloudflare account ID"))
        .await?;
    let mut env = vec![(CLOUDFLARE_ACCOUNT_ID, account)];
    if gateway {
        let id = interaction
            .prompt(text("Enter Cloudflare AI Gateway ID"))
            .await?;
        env.push((CLOUDFLARE_GATEWAY_ID, id));
    }
    Ok(ApiKeyCredential {
        key: Some(key),
        env: env_of(&env),
    })
}

/// Replaces Cloudflare's `{CLOUDFLARE_ACCOUNT_ID}` and
/// `{CLOUDFLARE_GATEWAY_ID}` placeholders in a base URL with the request's
/// provider settings. Port of `resolveCloudflareModel`.
pub fn cloudflare_base_url(base_url: &str, env: Option<&ProviderEnv>) -> String {
    let Some(env) = env else {
        return base_url.to_owned();
    };
    let mut url = base_url.to_owned();
    for name in [CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_GATEWAY_ID] {
        if let Some(value) = env.get(name) {
            url = url.replace(&format!("{{{name}}}"), value);
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ambient<'a>(
        vars: &'a [(&'a str, &'a str)],
        files: &'a [&'a str],
    ) -> (
        impl Fn(&str) -> Option<String> + 'a,
        impl Fn(&str) -> bool + 'a,
    ) {
        (
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            },
            move |path: &str| files.contains(&path),
        )
    }

    fn stored(key: Option<&str>, env: &[(&str, &str)]) -> ApiKeyCredential {
        ApiKeyCredential {
            key: key.map(str::to_owned),
            env: (!env.is_empty()).then(|| {
                env.iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect()
            }),
        }
    }

    fn run(
        provider: &str,
        credential: Option<&ApiKeyCredential>,
        vars: &[(&str, &str)],
        files: &[&str],
    ) -> Option<Resolved> {
        let (env, exists) = ambient(vars, files);
        resolve(
            provider,
            credential,
            &Ambient {
                env: &env,
                file_exists: &exists,
            },
        )
    }

    #[test]
    fn llama_reads_the_server_and_key_like_pi() {
        assert_eq!(run("llama.cpp", None, &[], &[]), None);
        let from_env = run(
            "llama.cpp",
            None,
            &[("LLAMA_BASE_URL", "http://box:8080/v1/")],
            &[],
        )
        .unwrap();
        assert_eq!(from_env.source, "LLAMA_BASE_URL");
        assert_eq!(from_env.api_key.as_deref(), Some("local"));
        assert_eq!(from_env.base_url.as_deref(), Some("http://box:8080/v1"));
        let credential = stored(Some("k"), &[("LLAMA_BASE_URL", "http://h:1")]);
        let stored = run(
            "llama.cpp",
            Some(&credential),
            &[
                ("LLAMA_BASE_URL", "http://other:2"),
                ("LLAMA_API_KEY", "env"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(stored.source, "stored credential");
        assert_eq!(stored.api_key.as_deref(), Some("k"));
        assert_eq!(stored.base_url.as_deref(), Some("http://h:1/v1"));
        assert_eq!(stored.env.unwrap()["LLAMA_BASE_URL"], "http://h:1");
    }

    #[test]
    fn bedrock_follows_pi_order() {
        let source = |credential: Option<&ApiKeyCredential>, vars: &[(&str, &str)]| {
            run("amazon-bedrock", credential, vars, &[]).map(|r| r.source)
        };
        let token = stored(Some("tok"), &[("AWS_REGION", "eu-west-1")]);
        let resolved = run("amazon-bedrock", Some(&token), &[], &[]).unwrap();
        assert_eq!(resolved.api_key.as_deref(), Some("tok"));
        assert_eq!(resolved.env.unwrap()["AWS_REGION"], "eu-west-1");
        assert_eq!(
            source(
                None,
                &[("AWS_BEARER_TOKEN_BEDROCK", "b"), ("AWS_PROFILE", "p")]
            )
            .as_deref(),
            Some("AWS_BEARER_TOKEN_BEDROCK")
        );
        let profile = stored(None, &[("AWS_PROFILE", "dev")]);
        let resolved = run("amazon-bedrock", Some(&profile), &[], &[]).unwrap();
        assert_eq!(resolved.source, "stored credential");
        assert_eq!(resolved.api_key, None);
        assert_eq!(resolved.env.unwrap()["AWS_PROFILE"], "dev");
        assert_eq!(
            source(None, &[("AWS_ACCESS_KEY_ID", "a")]),
            None,
            "a key id needs its secret"
        );
        assert_eq!(
            source(
                None,
                &[("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "s")]
            )
            .as_deref(),
            Some("AWS access keys")
        );
        assert_eq!(
            source(None, &[("AWS_CONTAINER_CREDENTIALS_FULL_URI", "u")]).as_deref(),
            Some("ECS task role")
        );
        assert_eq!(
            source(None, &[("AWS_WEB_IDENTITY_TOKEN_FILE", "f")]).as_deref(),
            Some("web identity token")
        );
        // An empty stored credential, from the credential chain login, still
        // resolves from the environment.
        let chain = stored(None, &[]);
        assert_eq!(
            source(Some(&chain), &[("AWS_PROFILE", "p")]).as_deref(),
            Some("AWS_PROFILE")
        );
    }

    #[test]
    fn vertex_needs_credentials_project_and_location() {
        let full = [
            ("GOOGLE_CLOUD_PROJECT", "p"),
            ("GOOGLE_CLOUD_LOCATION", "us-central1"),
        ];
        assert_eq!(
            run("google-vertex", None, &[("GOOGLE_CLOUD_API_KEY", "k")], &[])
                .unwrap()
                .api_key
                .as_deref(),
            Some("k")
        );
        assert!(run("google-vertex", None, &full, &[]).is_none());
        let adc = run("google-vertex", None, &full, &[VERTEX_ADC_PATH]).unwrap();
        assert_eq!(adc.source, "gcloud application default credentials");
        assert_eq!(adc.api_key, None);
        assert!(
            run(
                "google-vertex",
                None,
                &[("GCLOUD_PROJECT", "p")],
                &[VERTEX_ADC_PATH]
            )
            .is_none()
        );
        let service = stored(
            None,
            &[
                ("GOOGLE_APPLICATION_CREDENTIALS", "/sa.json"),
                ("GOOGLE_CLOUD_PROJECT", "p"),
                ("GOOGLE_CLOUD_LOCATION", "global"),
            ],
        );
        let resolved = run("google-vertex", Some(&service), &[], &["/sa.json"]).unwrap();
        assert_eq!(resolved.source, "stored credential");
        assert_eq!(resolved.env.unwrap()["GOOGLE_CLOUD_LOCATION"], "global");
    }

    #[test]
    fn cloudflare_merges_fields_and_moves_the_gateway_key_to_a_header() {
        let partial = stored(Some("cf"), &[]);
        assert!(run("cloudflare-workers-ai", Some(&partial), &[], &[]).is_none());
        let workers = run(
            "cloudflare-workers-ai",
            Some(&partial),
            &[("CLOUDFLARE_ACCOUNT_ID", "acct")],
            &[],
        )
        .unwrap();
        assert_eq!(workers.api_key.as_deref(), Some("cf"));
        assert_eq!(workers.env.unwrap()["CLOUDFLARE_ACCOUNT_ID"], "acct");
        assert_eq!(workers.source, "stored credential");
        let vars = [
            ("CLOUDFLARE_API_KEY", "env-key"),
            ("CLOUDFLARE_ACCOUNT_ID", "acct"),
        ];
        assert!(run("cloudflare-ai-gateway", None, &vars, &[]).is_none());
        let gateway = run(
            "cloudflare-ai-gateway",
            None,
            &[
                ("CLOUDFLARE_API_KEY", "env-key"),
                ("CLOUDFLARE_ACCOUNT_ID", "acct"),
                ("CLOUDFLARE_GATEWAY_ID", "gw"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(gateway.api_key, None);
        assert_eq!(gateway.source, "CLOUDFLARE_API_KEY");
        assert_eq!(
            gateway.headers.get("cf-aig-authorization"),
            Some(&Some("Bearer env-key".to_owned()))
        );
        assert_eq!(gateway.headers.get("Authorization"), Some(&None));
        let env = gateway.env.unwrap();
        assert_eq!(
            cloudflare_base_url(
                "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai",
                Some(&env)
            ),
            "https://gateway.ai.cloudflare.com/v1/acct/gw/openai"
        );
    }

    #[test]
    fn federation_needs_all_three_ids() {
        let (env, exists) = ambient(
            &[
                ("ANTHROPIC_FEDERATION_RULE_ID", "rule"),
                ("ANTHROPIC_ORGANIZATION_ID", "org"),
                ("ANTHROPIC_IDENTITY_TOKEN_FILE", "/token"),
                ("ANTHROPIC_WORKSPACE_ID", "ws"),
            ],
            &[],
        );
        let resolved = anthropic_federation(&Ambient {
            env: &env,
            file_exists: &exists,
        })
        .unwrap();
        let env = resolved.env.unwrap();
        assert_eq!(env.len(), 4);
        assert_eq!(env["ANTHROPIC_WORKSPACE_ID"], "ws");
        let (env, exists) = ambient(&[("ANTHROPIC_FEDERATION_RULE_ID", "rule")], &[]);
        assert!(
            anthropic_federation(&Ambient {
                env: &env,
                file_exists: &exists
            })
            .is_none()
        );
    }

    #[tokio::test]
    async fn logins_ask_pi_questions() {
        use crate::auth::AuthRequest;
        use tokio_util::sync::CancellationToken;
        let answers = [
            (
                "Select Google Vertex AI authentication method:",
                "service-account",
            ),
            ("Enter Google Cloud project ID", "proj"),
            ("Enter Google Cloud location", "us-east5"),
            ("Enter service account credentials file path", "/sa.json"),
        ];
        let (interaction, mut requests) = Interaction::new(CancellationToken::new());
        let answering = tokio::spawn(async move {
            let mut asked = Vec::new();
            while let Some(request) = requests.recv().await {
                if let AuthRequest::Prompt { prompt, reply, .. } = request {
                    let message = match &prompt {
                        AuthPrompt::Select { message, .. }
                        | AuthPrompt::Text { message, .. }
                        | AuthPrompt::Secret { message }
                        | AuthPrompt::ManualCode { message, .. } => message.clone(),
                    };
                    let answer = answers
                        .iter()
                        .find(|(question, _)| *question == message)
                        .map(|(_, answer)| *answer)
                        .unwrap();
                    asked.push(message);
                    reply.send(answer.to_owned()).unwrap();
                }
            }
            asked
        });
        let credential = login("google-vertex", &interaction).await.unwrap().unwrap();
        drop(interaction);
        assert_eq!(answering.await.unwrap().len(), 4);
        assert_eq!(credential.key, None);
        let env = credential.env.unwrap();
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            [
                "GOOGLE_CLOUD_PROJECT",
                "GOOGLE_CLOUD_LOCATION",
                "GOOGLE_APPLICATION_CREDENTIALS"
            ]
        );
        assert!(
            login("openai", &Interaction::new(CancellationToken::new()).0)
                .await
                .is_none()
        );
    }
}
