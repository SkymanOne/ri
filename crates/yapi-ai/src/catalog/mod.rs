//! The built-in model catalog, generated from pi-ai's data by `cargo xtask models`.

mod data;

pub(crate) use data::GENERATED_AT;

use serde::de::DeserializeOwned;
use yapi_types::model::{ClassifierModel, ImageModel, Model};

/// Built-in provider ids, in catalog order.
pub fn builtin_providers() -> impl Iterator<Item = &'static str> {
    data::PROVIDERS.iter().map(|(id, _)| *id)
}

/// The built-in chat models of a provider, in catalog order; empty for an unknown
/// provider. Each call parses that provider's data.
pub fn builtin_models(provider: &str) -> Vec<Model> {
    data::PROVIDERS
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, json)| parse(json))
        .unwrap_or_default()
}

/// Every built-in chat model, in catalog order.
pub fn all_builtin_models() -> Vec<Model> {
    parse_all(data::PROVIDERS)
}

/// Every built-in classifier model, in catalog order.
pub fn all_builtin_classifiers() -> Vec<ClassifierModel> {
    parse_all(data::CLASSIFIERS)
}

/// Every built-in image-generation model, in catalog order.
pub fn all_builtin_image_models() -> Vec<ImageModel> {
    parse_all(data::IMAGES)
}

fn parse_all<T: DeserializeOwned>(data: &[(&str, &str)]) -> Vec<T> {
    data.iter().flat_map(|(_, json)| parse(json)).collect()
}

fn parse<T: DeserializeOwned>(json: &str) -> Vec<T> {
    // The data is generated and checked by the catalog test below.
    serde_json::from_str(json).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_parses() {
        for (provider, json) in data::PROVIDERS {
            let models: Vec<Model> = serde_json::from_str(json)
                .unwrap_or_else(|err| panic!("catalog data for {provider}: {err}"));
            let others = data::CLASSIFIERS
                .iter()
                .chain(data::IMAGES)
                .any(|(id, _)| id == provider);
            assert!(!models.is_empty() || others, "{provider} has no models");
            assert!(models.iter().all(|model| model.provider == *provider));
        }
        for (provider, json) in data::CLASSIFIERS {
            let models: Vec<ClassifierModel> = serde_json::from_str(json)
                .unwrap_or_else(|err| panic!("classifier data for {provider}: {err}"));
            assert!(models.iter().all(|model| model.provider == *provider));
        }
        for (provider, json) in data::IMAGES {
            let models: Vec<ImageModel> = serde_json::from_str(json)
                .unwrap_or_else(|err| panic!("image data for {provider}: {err}"));
            assert!(models.iter().all(|model| model.provider == *provider));
        }
        assert!(
            all_builtin_classifiers()
                .iter()
                .any(|model| model.provider == "typesafe" && model.id == "jev-latest")
        );
        assert!(!all_builtin_image_models().is_empty());
    }

    #[test]
    fn finds_known_models() {
        let anthropic = builtin_models("anthropic");
        assert!(
            anthropic
                .iter()
                .any(|model| model.id == "claude-sonnet-4-5")
        );
        let go = builtin_models("opencode-go");
        let apis: std::collections::BTreeSet<_> =
            go.iter().map(|model| model.api.as_str()).collect();
        assert_eq!(
            apis.into_iter().collect::<Vec<_>>(),
            [
                "anthropic-messages",
                "openai-completions",
                "openai-responses"
            ]
        );
        assert!(builtin_models("nope").is_empty());
    }
}
