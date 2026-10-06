//! What to tell the user when no model or credential is usable.
//!
//! Port of `packages/coding-agent/src/core/auth-guidance.ts` in pi `v1.0.0`.
//! pi points to the docs in its install; yapi ships none, so it points to its
//! documentation site.

/// Where yapi documents providers and sign-in.
pub const PROVIDER_DOCS: &str = "https://skymanone.github.io/yapi/models.html";

/// pi's `getProviderLoginHelp`.
pub fn provider_login_help() -> String {
    format!("Use /login to log into a provider via OAuth or API key. See:\n  {PROVIDER_DOCS}")
}

/// pi's `formatNoModelsAvailableMessage`.
pub fn no_models_available() -> String {
    format!("No models available. {}", provider_login_help())
}

/// pi's `formatNoModelSelectedMessage`.
pub fn no_model_selected() -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        provider_login_help()
    )
}

/// pi's `formatNoApiKeyFoundMessage`.
pub fn no_api_key_found(provider: &str) -> String {
    let provider = if provider == "unknown" {
        "the selected model"
    } else {
        provider
    };
    format!(
        "No API key found for {provider}.\n\n{}",
        provider_login_help()
    )
}

#[cfg(test)]
mod tests {
    /// The link names a page that the documentation site builds from `docs/`.
    #[test]
    fn provider_docs_is_a_docs_page() {
        let page = super::PROVIDER_DOCS
            .strip_prefix("https://skymanone.github.io/yapi/")
            .and_then(|page| page.strip_suffix(".html"))
            .unwrap();
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        let summary = std::fs::read_to_string(docs.join("SUMMARY.md")).unwrap();
        assert!(summary.contains(&format!("]({page}.md)")), "{page}");
    }
}
