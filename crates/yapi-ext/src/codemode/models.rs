//! `models.*` for scripts: the model catalog, classifiers and image models
//! with the session's credentials. Port of `createModelGlobals` and its
//! checks in `extensions/codemode/execute.ts` in pi-coding-agent `v1.0.0`.

use serde_json::{Map, Value, json};
use yapi_ai::registry::ModelRegistry;

/// `models.classify()` and `models.generateImages()` calls one script may
/// have in flight; more wait for a slot.
pub(super) const MAX_CONCURRENT_MODEL_CALLS: usize = 4;

/// The globals scripts see, in pi's order.
pub(super) const GLOBALS: [&str; 5] = [
    "models.getModelsOfType",
    "models.getAvailableOfType",
    "models.getModelOfType",
    "models.classify",
    "models.generateImages",
];

const MODEL_TYPES: [&str; 3] = ["chat", "image", "classifier"];

const CLASSIFIER_CONTEXT_SHAPE: &str = r#"{ state: { ... }, questions: { <id>: { type: "choice", instructions, criteria: { <label>: <meaning> } } | { type: "score", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: "bool", instructions, criteria: { true: <meaning>, false: <meaning> } } } }"#;

/// A JSON value as `JSON.stringify` shows it in a message.
fn stringify(value: &Value) -> String {
    yapi_types::json::to_string(value).unwrap_or_default()
}

pub(crate) fn model_type(value: &Value) -> Result<&'static str, String> {
    value
        .as_str()
        .and_then(|kind| MODEL_TYPES.iter().copied().find(|known| *known == kind))
        .ok_or_else(|| {
            format!(
                "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
                stringify(value)
            )
        })
}

pub(super) fn provider(value: &Value) -> Result<Option<&str>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(provider) => Ok(Some(provider)),
        _ => Err("provider must be a string".into()),
    }
}

/// `an image`, `a classifier`.
fn with_article(word: &str) -> String {
    let article = if word.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {word}")
}

/// A script value in an error message; pi's `describeValue`.
pub(super) fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Array(items) if items.is_empty() => "an empty array".into(),
        Value::Array(_) => "an array".into(),
        Value::Object(object) if object.is_empty() => "{}".into(),
        Value::Object(object) => {
            let keys: Vec<&str> = object.keys().map(String::as_str).collect();
            let more = if keys.len() > 6 { ", ..." } else { "" };
            format!("{{ {}{more} }}", keys[..keys.len().min(6)].join(", "))
        }
        Value::String(_) => "a string".into(),
        Value::Bool(_) => "a boolean".into(),
        Value::Number(_) => "a number".into(),
    }
}

/// A property for messages: a missing one reads as `undefined`, as after
/// pi's JSON round trip.
fn describe_field(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_owned(), describe)
}

/// A catalog entry for scripts, without `headers`, which can carry
/// credentials from `models.json`.
fn info(mut model: Value) -> Value {
    if let Some(object) = model.as_object_mut() {
        object.remove("headers");
    }
    model
}

fn to_value(model: &impl serde::Serialize) -> Value {
    serde_json::to_value(model).unwrap_or(Value::Null)
}

/// pi's `classify` (for a `"classifier"`) or `generateImages` on the model of
/// `kind` named by `provider` and `id`: the result as JSON.
pub(crate) async fn run_model(
    registry: &ModelRegistry,
    kind: &str,
    (provider, id): (&str, &str),
    context: &Value,
    temperature: Option<f64>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<Value, String> {
    let context = context.clone();
    let result = if kind == "classifier" {
        let model = registry
            .classifiers()
            .iter()
            .find(|model| model.provider == provider && model.id == id)
            .cloned()
            .ok_or_else(|| format!("Unknown classifier model \"{provider}/{id}\""))?;
        let context = serde_json::from_value(context).map_err(|err| err.to_string())?;
        let options = yapi_ai::api::classify::ClassifyOptions {
            temperature,
            cancel,
            ..Default::default()
        };
        serde_json::to_value(registry.classify(&model, &context, options).await)
    } else {
        let model = registry
            .image_models()
            .iter()
            .find(|model| model.provider == provider && model.id == id)
            .cloned()
            .ok_or_else(|| format!("Unknown image model \"{provider}/{id}\""))?;
        let context = serde_json::from_value(context).map_err(|err| err.to_string())?;
        let options = yapi_ai::api::images::ImagesOptions {
            cancel,
            ..Default::default()
        };
        serde_json::to_value(registry.generate_images(&model, &context, options).await)
    };
    result.map_err(|err| err.to_string())
}

/// Every model of `kind`, optionally of one provider.
pub(crate) fn models_of_type(
    registry: &ModelRegistry,
    kind: &str,
    provider: Option<&str>,
    available: bool,
) -> Vec<Value> {
    let models: Vec<Value> = match (kind, available) {
        ("chat", true) => registry.available().into_iter().map(to_value).collect(),
        ("chat", false) => registry.models().iter().map(to_value).collect(),
        ("classifier", true) => registry
            .available_classifiers()
            .into_iter()
            .map(to_value)
            .collect(),
        ("classifier", false) => registry.classifiers().iter().map(to_value).collect(),
        (_, true) => registry
            .available_image_models()
            .into_iter()
            .map(to_value)
            .collect(),
        (_, false) => registry.image_models().iter().map(to_value).collect(),
    };
    models
        .into_iter()
        .filter(|model| provider.is_none_or(|provider| model["provider"] == provider))
        .map(info)
        .collect()
}

/// One catalog entry of `kind`.
pub(super) fn model_of_type(
    registry: &ModelRegistry,
    kind: &str,
    provider: &str,
    id: &str,
) -> Option<Value> {
    models_of_type(registry, kind, Some(provider), false)
        .into_iter()
        .find(|model| model["id"] == id)
}

/// The `provider` and `id` of the script's model argument, resolved to a
/// catalog entry of `kind`; a script-supplied base URL or headers never
/// receive the credentials.
pub(super) fn resolve_model(
    registry: &ModelRegistry,
    name: &str,
    kind: &str,
    model: &Value,
) -> Result<(String, String), String> {
    let hint =
        format!("List the {kind} models you can use with models.getAvailableOfType(\"{kind}\").");
    let (Some(provider), Some(id)) = (model["provider"].as_str(), model["id"].as_str()) else {
        // undefined arrives as null: spread arguments cross as a JSON array.
        let undefined = if model.is_null() {
            " models.getModelOfType() returns undefined for an unknown provider or id."
        } else {
            ""
        };
        return Err(format!(
            "{name}() expects {} model as its first argument, got {}.{undefined} {hint}",
            with_article(kind),
            describe(model)
        ));
    };
    if model_of_type(registry, kind, provider, id).is_none() {
        let reference = format!("{provider}/{id}");
        let actual = MODEL_TYPES.iter().find(|other| {
            **other != kind && model_of_type(registry, other, provider, id).is_some()
        });
        return Err(match actual {
            Some(actual) => format!(
                "\"{reference}\" is {} model, not {} model. {hint}",
                with_article(actual),
                with_article(kind)
            ),
            None => format!("Unknown {kind} model \"{reference}\". {hint}"),
        });
    }
    Ok((provider.to_owned(), id.to_owned()))
}

fn is_strings(values: &[&Value]) -> bool {
    !values.is_empty() && values.iter().all(|value| value.is_string())
}

/// pi's `checkClassifierContext`: the script's context, or why it is not one.
pub(super) fn check_classifier_context(context: &Value, docs: &str) -> Result<(), String> {
    let fail = |problem: String| {
        format!(
            "models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}. See \"Classify\" in {docs}."
        )
    };
    let Some(object) = context.as_object() else {
        return Err(fail(format!(
            "expects a context object as its second argument, got {}",
            describe(context)
        )));
    };
    let state = object.get("state");
    if !state.is_some_and(Value::is_object) {
        return Err(fail(format!(
            "context.state must be an object, got {}",
            describe_field(state)
        )));
    }
    let questions = object.get("questions");
    let Some(questions) = questions
        .and_then(Value::as_object)
        .filter(|questions| !questions.is_empty())
    else {
        return Err(fail(format!(
            "context.questions must map question IDs to questions, got {}",
            describe_field(questions)
        )));
    };
    for (id, question) in questions {
        let at = format!("context.questions.{id}");
        let Some(question) = question.as_object() else {
            return Err(fail(format!(
                "{at} must be a question object, got {}",
                describe(question)
            )));
        };
        if !question.get("instructions").is_some_and(Value::is_string) {
            return Err(fail(format!("{at}.instructions must be a string")));
        }
        let criteria = question.get("criteria").unwrap_or(&Value::Null);
        match question.get("type").and_then(Value::as_str) {
            Some("choice") => {
                let valid = criteria
                    .as_object()
                    .is_some_and(|criteria| is_strings(&criteria.values().collect::<Vec<_>>()));
                if !valid {
                    return Err(fail(format!(
                        "{at} is a \"choice\" question, so criteria must map each label to its meaning"
                    )));
                }
            }
            Some("score") => {
                let valid = criteria
                    .as_array()
                    .is_some_and(|criteria| is_strings(&criteria.iter().collect::<Vec<_>>()));
                if !valid {
                    return Err(fail(format!(
                        "{at} is a \"score\" question, so criteria must list the levels as strings, lowest first"
                    )));
                }
            }
            Some("bool") => {
                let valid = criteria["true"].is_string() && criteria["false"].is_string();
                if !valid {
                    return Err(fail(format!(
                        "{at} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"
                    )));
                }
            }
            _ => {
                let kind = question
                    .get("type")
                    .map_or("undefined".to_owned(), stringify);
                return Err(fail(format!(
                    "{at}.type must be \"choice\", \"score\", or \"bool\", got {kind}"
                )));
            }
        }
    }
    Ok(())
}

/// pi's `checkImagesContext`.
pub(super) fn check_images_context(context: &Value, docs: &str) -> Result<(), String> {
    let fail = |problem: String| {
        format!(
            "models.generateImages() {problem}. Expected context: {{ input: [{{ type: \"text\", text: <prompt> }}, ...optional {{ type: \"image\", data: <base64>, mimeType }} references] }}. See \"Generate images\" in {docs}."
        )
    };
    let Some(object) = context.as_object() else {
        return Err(fail(format!(
            "expects a context object as its second argument, got {}",
            describe(context)
        )));
    };
    let input = object.get("input");
    let Some(blocks) = input
        .and_then(Value::as_array)
        .filter(|blocks| !blocks.is_empty())
    else {
        return Err(fail(format!(
            "context.input must be a non-empty array of blocks, got {}",
            describe_field(input)
        )));
    };
    for (index, block) in blocks.iter().enumerate() {
        let text = block["type"] == "text" && block["text"].is_string();
        let image =
            block["type"] == "image" && block["data"].is_string() && block["mimeType"].is_string();
        if !(text || image) {
            return Err(fail(format!(
                "context.input[{index}] must be a text or image block, got {}",
                describe(block)
            )));
        }
    }
    Ok(())
}

/// The arguments of a spread global, as a list.
pub(super) fn args(args: &Value) -> Vec<Value> {
    match args {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

/// `models.getModelOfType()`'s check that the provider and id are strings.
pub(super) fn model_of_type_args(items: &[Value]) -> Result<(&str, &str), String> {
    match (
        items.get(1).and_then(Value::as_str),
        items.get(2).and_then(Value::as_str),
    ) {
        (Some(provider), Some(id)) => Ok((provider, id)),
        _ => Err(format!(
            "models.getModelOfType(type, provider, id) expects three strings, got ({}). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\").",
            items.iter().map(describe).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The note pi adds when a script never shows the images it generated.
pub(super) fn unshown_images_note(count: usize) -> Value {
    let plural = if count == 1 { "" } else { "s" };
    json!({
        "type": "text",
        "text": format!("Note: models.generateImages() returned {count} image{plural} that the script did not show. Show each image block of result.output with image(block)."),
    })
}

/// A result's `stopReason` as a call row status.
pub(super) fn status(result: &Map<String, Value>) -> &'static str {
    match result.get("stopReason").and_then(Value::as_str) {
        Some("stop") => "ok",
        Some("aborted") => "cancelled",
        _ => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_values_like_pi() {
        assert_eq!(describe(&json!({"prompt": "x"})), "{ prompt }");
        assert_eq!(describe(&json!([])), "an empty array");
        assert_eq!(describe(&json!("x")), "a string");
        assert_eq!(
            model_type(&json!("video")).unwrap_err(),
            "Unknown model type \"video\". Use \"chat\", \"image\", or \"classifier\"."
        );
        assert_eq!(with_article("image"), "an image");
    }

    #[test]
    fn checks_contexts_like_pi() {
        let error = check_images_context(&json!({"prompt": "a cat"}), "D").unwrap_err();
        assert!(error.starts_with(
            "models.generateImages() context.input must be a non-empty array of blocks, got undefined. Expected context: "
        ));
        let error = check_classifier_context(
            &json!({"state": {}, "questions": {"q": {"type": "bool", "instructions": "x", "criteria": {"true": "y"}}}}),
            "D",
        )
        .unwrap_err();
        assert!(error.starts_with(
            "models.classify() context.questions.q is a \"bool\" question, so criteria must be { true: string, false: string }."
        ));
    }
}
