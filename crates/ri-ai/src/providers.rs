//! Built-in provider names and the authentication methods each declares.
//!
//! Mirrors the `createProvider` definitions in `packages/ai/src/providers` of
//! pi `v1.0.0`.

/// A built-in provider's display name and sign-in methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Provider id, as in the catalog.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// API key method, when the provider accepts keys.
    pub api_key: Option<ApiKeyMethod>,
    /// OAuth method name, when pi offers an account sign-in for the provider.
    pub oauth: Option<&'static str>,
}

/// How a provider takes an API key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiKeyMethod {
    /// Method name, shown in prompts as `Enter <name>`.
    pub name: &'static str,
    /// Environment variables holding the key, in priority order.
    pub env: &'static [&'static str],
    /// Whether `/login` can store a credential; otherwise the provider is
    /// configured outside ri.
    pub login: bool,
}

const fn key(name: &'static str, env: &'static [&'static str]) -> Option<ApiKeyMethod> {
    Some(ApiKeyMethod {
        name,
        env,
        login: true,
    })
}

const fn provider(
    id: &'static str,
    name: &'static str,
    api_key: Option<ApiKeyMethod>,
    oauth: Option<&'static str>,
) -> ProviderInfo {
    ProviderInfo {
        id,
        name,
        api_key,
        oauth,
    }
}

/// Every built-in provider.
pub const PROVIDERS: &[ProviderInfo] = &[
    provider(
        "amazon-bedrock",
        "Amazon Bedrock",
        key("AWS credentials or bearer token", &[]),
        None,
    ),
    provider(
        "ant-ling",
        "Ant Ling",
        key("Ant Ling API key", &["ANT_LING_API_KEY"]),
        None,
    ),
    provider(
        "anthropic",
        "Anthropic",
        key(
            "Anthropic API key",
            &[
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_API_KEY",
            ],
        ),
        Some("Anthropic (Claude Pro/Max)"),
    ),
    provider(
        "azure-openai-responses",
        "Azure OpenAI",
        key("Azure OpenAI API key", &["AZURE_OPENAI_API_KEY"]),
        None,
    ),
    provider(
        "baseten",
        "Baseten",
        key("Baseten API key", &["BASETEN_API_KEY"]),
        None,
    ),
    provider(
        "cerebras",
        "Cerebras",
        key("Cerebras API key", &["CEREBRAS_API_KEY"]),
        None,
    ),
    provider(
        "cloudflare-ai-gateway",
        "Cloudflare AI Gateway",
        key("Cloudflare API key", &["CLOUDFLARE_API_KEY"]),
        None,
    ),
    provider(
        "cloudflare-workers-ai",
        "Cloudflare Workers AI",
        key("Cloudflare API key", &["CLOUDFLARE_API_KEY"]),
        None,
    ),
    provider(
        "deepseek",
        "DeepSeek",
        key("DeepSeek API key", &["DEEPSEEK_API_KEY"]),
        None,
    ),
    provider(
        "fireworks",
        "Fireworks",
        key("Fireworks API key", &["FIREWORKS_API_KEY"]),
        None,
    ),
    provider(
        "github-copilot",
        "GitHub Copilot",
        key("GitHub Copilot token", &["COPILOT_GITHUB_TOKEN"]),
        Some("GitHub Copilot"),
    ),
    provider(
        "google",
        "Google",
        key("Gemini API key", &["GEMINI_API_KEY"]),
        None,
    ),
    provider(
        "google-vertex",
        "Google Vertex AI",
        key("Google Cloud credentials", &["GOOGLE_CLOUD_API_KEY"]),
        None,
    ),
    provider("groq", "Groq", key("Groq API key", &["GROQ_API_KEY"]), None),
    provider(
        "huggingface",
        "Hugging Face",
        key("Hugging Face token", &["HF_TOKEN"]),
        None,
    ),
    provider(
        "kimi-coding",
        "Kimi For Coding",
        key("Kimi API key", &["KIMI_API_KEY"]),
        Some("Kimi Code (subscription)"),
    ),
    provider(
        "meta",
        "Meta",
        key("Meta Model API key", &["META_API_KEY"]),
        Some("Meta (Muse subscription)"),
    ),
    provider(
        "minimax",
        "MiniMax",
        key("MiniMax API key", &["MINIMAX_API_KEY"]),
        None,
    ),
    provider(
        "minimax-cn",
        "MiniMax CN",
        key("MiniMax CN API key", &["MINIMAX_CN_API_KEY"]),
        None,
    ),
    provider(
        "mistral",
        "Mistral",
        key("Mistral API key", &["MISTRAL_API_KEY"]),
        None,
    ),
    provider(
        "moonshotai",
        "Moonshot AI",
        key("Moonshot AI API key", &["MOONSHOT_API_KEY"]),
        None,
    ),
    provider(
        "moonshotai-cn",
        "Moonshot AI CN",
        key("Moonshot AI API key", &["MOONSHOT_API_KEY"]),
        None,
    ),
    provider(
        "nvidia",
        "NVIDIA",
        key("NVIDIA API key", &["NVIDIA_API_KEY"]),
        None,
    ),
    provider(
        "openai",
        "OpenAI",
        key("OpenAI API key", &["OPENAI_API_KEY"]),
        Some("OpenAI (ChatGPT subscription)"),
    ),
    provider(
        "openai-codex",
        "OpenAI Codex (legacy)",
        None,
        Some("OpenAI (ChatGPT Plus/Pro)"),
    ),
    provider(
        "opencode",
        "OpenCode Zen",
        key("OpenCode API key", &["OPENCODE_API_KEY"]),
        None,
    ),
    provider(
        "opencode-go",
        "OpenCode Go",
        key("OpenCode API key", &["OPENCODE_API_KEY"]),
        None,
    ),
    provider(
        "openrouter",
        "OpenRouter",
        key("OpenRouter API key", &["OPENROUTER_API_KEY"]),
        Some("OpenRouter OAuth"),
    ),
    provider(
        "qwen-token-plan",
        "Qwen Token Plan",
        key("Qwen Token Plan API key", &["QWEN_TOKEN_PLAN_API_KEY"]),
        None,
    ),
    provider(
        "qwen-token-plan-cn",
        "Qwen Token Plan CN",
        key(
            "Qwen Token Plan CN API key",
            &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        ),
        None,
    ),
    provider(
        "qwen-token-plan-individual",
        "Qwen Token Plan Individual",
        key(
            "Qwen Token Plan Individual API key",
            &["QWEN_TOKEN_PLAN_API_KEY"],
        ),
        None,
    ),
    provider(
        "radius",
        "Radius",
        key("Radius API key", &["RADIUS_API_KEY"]),
        Some("Radius"),
    ),
    provider(
        "together",
        "Together",
        key("Together API key", &["TOGETHER_API_KEY"]),
        None,
    ),
    provider(
        "typesafe",
        "TypeSafe",
        key("TypeSafe API key", &["TYPESAFE_API_KEY"]),
        None,
    ),
    provider(
        "vercel-ai-gateway",
        "Vercel AI Gateway",
        key("Vercel AI Gateway API key", &["AI_GATEWAY_API_KEY"]),
        None,
    ),
    provider(
        "xai",
        "xAI",
        key("xAI API key", &["XAI_API_KEY"]),
        Some("xAI (Grok/X subscription)"),
    ),
    provider(
        "xiaomi",
        "Xiaomi",
        key("Xiaomi API key", &["XIAOMI_API_KEY"]),
        None,
    ),
    provider(
        "xiaomi-token-plan-ams",
        "Xiaomi Token Plan AMS",
        key(
            "Xiaomi Token Plan AMS API key",
            &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        ),
        None,
    ),
    provider(
        "xiaomi-token-plan-cn",
        "Xiaomi Token Plan CN",
        key(
            "Xiaomi Token Plan CN API key",
            &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        ),
        None,
    ),
    provider(
        "xiaomi-token-plan-sgp",
        "Xiaomi Token Plan SGP",
        key(
            "Xiaomi Token Plan SGP API key",
            &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        ),
        None,
    ),
    provider("zai", "Z.AI", key("Z.AI API key", &["ZAI_API_KEY"]), None),
    provider(
        "zai-coding-cn",
        "Z.AI Coding CN",
        key("Z.AI Coding CN API key", &["ZAI_CODING_CN_API_KEY"]),
        None,
    ),
];

/// A built-in provider by id.
pub fn info(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_the_catalog() {
        for id in crate::catalog::builtin_providers() {
            assert!(info(id).is_some(), "{id} has no provider info");
        }
    }
}
