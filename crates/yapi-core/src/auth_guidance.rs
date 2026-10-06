//! What to tell the user when no model or credential is usable.
//!
//! Port of `packages/coding-agent/src/core/auth-guidance.ts` in pi `v1.0.0`.

use crate::docs::Locations;

/// The page on providers and signing in to them, pi's
/// `join(getDocsPath(), "providers.md")`, where `docs` are.
pub fn providers_doc(docs: &Locations) -> String {
    format!("{}/providers.md", docs.pi_docs)
}

/// pi's `getProviderLoginHelp`.
pub fn provider_login_help(docs: &Locations) -> String {
    let docs = &docs.pi_docs;
    format!(
        "Use /login to log into a provider via OAuth or API key. See:\n  {docs}/providers.md\n  {docs}/models.md"
    )
}

/// pi's `formatNoModelsAvailableMessage`.
pub fn no_models_available(docs: &Locations) -> String {
    format!("No models available. {}", provider_login_help(docs))
}

/// pi's `formatNoModelSelectedMessage`.
pub fn no_model_selected(docs: &Locations) -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        provider_login_help(docs)
    )
}

/// pi's `formatNoApiKeyFoundMessage`.
pub fn no_api_key_found(provider: &str, docs: &Locations) -> String {
    let provider = if provider == "unknown" {
        "the selected model"
    } else {
        provider
    };
    format!(
        "No API key found for {provider}.\n\n{}",
        provider_login_help(docs)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The help names pi's providers and models pages where `docs` are.
    #[test]
    fn help_names_pi_providers_and_models_pages() {
        let docs = Locations {
            pi_docs: "/agent/docs/pi/docs".into(),
            ..Locations::default()
        };
        assert_eq!(
            provider_login_help(&docs),
            "Use /login to log into a provider via OAuth or API key. See:\n  /agent/docs/pi/docs/providers.md\n  /agent/docs/pi/docs/models.md"
        );
        assert_eq!(providers_doc(&docs), "/agent/docs/pi/docs/providers.md");
    }
}
