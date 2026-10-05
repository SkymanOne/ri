//! Strict JSON-schema tool parameters for providers with constrained sampling.

use serde_json::{Map, Value};
use yapi_types::message::ToolDeclaration;

/// Keywords no strict mode accepts.
const UNSUPPORTED_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

/// A provider's extra keyword restrictions: `(key, value)` is unsupported.
pub type KeywordCheck = fn(&str, &Value) -> bool;

/// The schema rewritten for strict mode: every property required, optional ones
/// nullable, no additional properties. `Err` names the unsupported construct.
pub fn make_strict(schema: &Value, check: Option<KeywordCheck>) -> Result<Value, String> {
    let mut schema = schema.clone();
    if !schema.is_object() {
        return Err("root schema must have type object".into());
    }
    make_node_strict(&mut schema, check)?;
    if schema.get("type") != Some(&Value::from("object")) {
        return Err("root schema must have type object".into());
    }
    Ok(schema)
}

fn is_structured(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    let types: Vec<&str> = match object.get("type") {
        Some(Value::String(kind)) => vec![kind],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    types.contains(&"object")
        || types.contains(&"array")
        || object.contains_key("properties")
        || object.contains_key("items")
}

fn allows_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    match object.get("type") {
        Some(Value::String(kind)) if kind == "null" => return true,
        Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == "null") => return true,
        _ => {}
    }
    if object.get("const") == Some(&Value::Null)
        || object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.contains(&Value::Null))
    {
        return true;
    }
    object
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|variants| variants.iter().any(allows_null))
}

fn make_node_strict(schema: &mut Value, check: Option<KeywordCheck>) -> Result<(), String> {
    let Some(object) = schema.as_object_mut() else {
        return Err("boolean schemas are unsupported".into());
    };
    for key in UNSUPPORTED_KEYS {
        if object.get(*key).is_some_and(|value| !value.is_null()) {
            return Err(format!("{key} schemas are unsupported"));
        }
    }
    if let Some(check) = check {
        for (key, value) in object.iter() {
            if check(key, value) {
                return Err(format!(
                    "{key}: {} is unsupported",
                    yapi_types::json::to_string(value).unwrap_or_default()
                ));
            }
        }
    }
    if let Some(any_of) = object.get_mut("anyOf") {
        let Some(variants) = any_of
            .as_array_mut()
            .filter(|variants| !variants.is_empty())
        else {
            return Err("anyOf must contain at least one schema".into());
        };
        for variant in variants {
            if is_structured(variant) {
                return Err("object and array unions are unsupported".into());
            }
            make_node_strict(variant, check)?;
        }
    }
    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".into());
        }
        make_node_strict(items, check)?;
    }

    let is_object = object.get("type") == Some(&Value::from("object"));
    if object.contains_key("properties") && !is_object {
        return Err("properties require type object".into());
    }
    if !is_object {
        return Ok(());
    }
    if object
        .get("additionalProperties")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err("schema-valued or true additionalProperties is unsupported".into());
    }
    let required: Vec<String> = match object.get("required") {
        None => Vec::new(),
        Some(Value::Array(keys)) if keys.iter().all(Value::is_string) => keys
            .iter()
            .filter_map(|key| key.as_str().map(str::to_owned))
            .collect(),
        Some(_) => return Err("object required must be a string array".into()),
    };
    let mut properties = match object.get("properties") {
        None => Map::new(),
        Some(Value::Object(properties)) => properties.clone(),
        Some(_) => return Err("object properties must be a schema map".into()),
    };
    if required.iter().any(|key| !properties.contains_key(key)) {
        return Err("required contains an unknown property".into());
    }
    let names: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
    for (key, property) in properties.iter_mut() {
        make_node_strict(property, check)?;
        if !required.contains(key) && !allows_null(property) {
            let original = std::mem::take(property);
            *property = serde_json::json!({ "anyOf": [original, { "type": "null" }] });
        }
    }
    if object.contains_key("properties") {
        object.insert("properties".into(), Value::Object(properties));
    }
    object.insert("required".into(), Value::Array(names));
    object.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

/// Whether to send `tool` with strict JSON-schema sampling: it asks for it, the
/// provider supports it and the schema can be made strict. `Err` when the tool
/// requires strict sampling that cannot be had.
pub fn strict_sampling(
    tool: &ToolDeclaration,
    supports_strict: bool,
    check: Option<KeywordCheck>,
) -> Result<bool, String> {
    let Some(config) = tool
        .constrained_sampling
        .as_ref()
        .and_then(Value::as_object)
    else {
        return Ok(false);
    };
    if config.get("type").and_then(Value::as_str) != Some("json_schema") {
        return Ok(false);
    }
    let required = config.get("strict").and_then(Value::as_str) == Some("require");
    if supports_strict {
        return match make_strict(&tool.parameters, check) {
            Ok(_) => Ok(true),
            Err(_) if !required => Ok(false),
            Err(reason) => Err(format!(
                "Tool \"{}\" requires JSON-schema constrained sampling, but {reason}.",
                tool.name
            )),
        };
    }
    if required {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(false)
}

/// The parameters to send: strict when `strict`, as declared otherwise.
pub fn tool_parameters(tool: &ToolDeclaration, strict: bool) -> Value {
    if strict {
        make_strict(&tool.parameters, None).unwrap_or_else(|_| tool.parameters.clone())
    } else {
        tool.parameters.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn makes_optional_properties_nullable() {
        let schema = json!({"type":"object","required":["path"],"properties":{
            "path":{"type":"string"},"limit":{"type":"number","description":"n"}}});
        let strict = make_strict(&schema, None).unwrap();
        assert_eq!(
            yapi_types::json::to_string(&strict).unwrap(),
            r#"{"type":"object","required":["path","limit"],"properties":{"path":{"type":"string"},"limit":{"anyOf":[{"type":"number","description":"n"},{"type":"null"}]}},"additionalProperties":false}"#
        );
    }

    #[test]
    fn rejects_unsupported_constructs() {
        assert!(
            make_strict(
                &json!({"type":"object","properties":{"a":{"oneOf":[]}}}),
                None
            )
            .is_err()
        );
        assert!(make_strict(&json!({"type":"object","additionalProperties":true}), None).is_err());
        assert!(make_strict(&json!({"type":"string"}), None).is_err());
    }
}
