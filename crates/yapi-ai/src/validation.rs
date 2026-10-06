//! Tool argument validation: coerce what models commonly get wrong, then check the
//! JSON schema with TypeBox's error messages.
//!
//! Port of `packages/ai/src/utils/validation.ts` in pi `v1.0.0`. The checker covers
//! the JSON Schema keywords tool schemas use; `$ref`, `format` and `pattern` are
//! accepted without checking.

use serde_json::{Map, Number, Value};
use yapi_types::message::{ToolCall, ToolDeclaration};

/// Validated, coerced arguments for `call`, or the error pi reports to the model.
pub fn validate_tool_arguments(tool: &ToolDeclaration, call: &ToolCall) -> Result<Value, String> {
    let mut args = Value::Object(call.arguments.clone());
    normalize_optional_nulls(&mut args, &tool.parameters);
    let args = coerce(args, &tool.parameters);
    let mut errors = Vec::new();
    check(&args, &tool.parameters, "", &mut errors);
    if errors.is_empty() {
        return Ok(args);
    }
    let lines: Vec<String> = errors
        .iter()
        .map(|(path, message)| format!("  - {path}: {message}"))
        .collect();
    let received = yapi_types::json::to_string_pretty(&call.arguments, "  ").unwrap_or_default();
    Err(format!(
        "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{received}",
        call.name,
        lines.join("\n")
    ))
}

/// The types a schema's `type` names: one string or an array of them.
pub(crate) fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// The schemas of an array's items: positional for a tuple `items`, else the one
/// `items` schema for every item.
fn item_schemas(schema: &Value) -> impl Iterator<Item = &Value> {
    let (tuple, single) = match schema.get("items") {
        Some(Value::Array(schemas)) => (schemas.as_slice(), None),
        single => (&[][..], single),
    };
    tuple.iter().chain(std::iter::from_fn(move || single))
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|n| n.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Value::from(value as i64)
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

fn coerce_primitive(value: &Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("number" | "integer", Value::Null) => Some(Value::from(0)),
        ("number" | "integer", Value::String(text)) if !text.trim().is_empty() => {
            let parsed: f64 = text.trim().parse().ok()?;
            (parsed.is_finite() && (kind == "number" || parsed.fract() == 0.0))
                .then(|| number(parsed))
        }
        ("number" | "integer", Value::Bool(flag)) => Some(Value::from(u8::from(*flag))),
        ("boolean", Value::Null) => Some(Value::Bool(false)),
        ("boolean", Value::String(text)) if text == "true" || text == "false" => {
            Some(Value::Bool(text == "true"))
        }
        ("boolean", Value::Number(n)) if n.as_f64() == Some(1.0) || n.as_f64() == Some(0.0) => {
            Some(Value::Bool(n.as_f64() == Some(1.0)))
        }
        ("string", Value::Null) => Some(Value::String(String::new())),
        ("string", Value::Number(n)) => Some(Value::String(yapi_types::json::to_string(n).ok()?)),
        ("string", Value::Bool(flag)) => Some(Value::String(flag.to_string())),
        ("null", Value::String(text)) if text.is_empty() => Some(Value::Null),
        ("null", Value::Bool(false)) => Some(Value::Null),
        ("null", Value::Number(n)) if n.as_f64() == Some(0.0) => Some(Value::Null),
        _ => None,
    }
}

fn is_valid(value: &Value, schema: &Value) -> bool {
    let mut errors = Vec::new();
    check(value, schema, "", &mut errors);
    errors.is_empty()
}

fn coerce_union(value: Value, schemas: &[Value]) -> Value {
    if schemas.iter().any(|schema| is_valid(&value, schema)) {
        return value;
    }
    for schema in schemas {
        let candidate = coerce(value.clone(), schema);
        if is_valid(&candidate, schema) {
            return candidate;
        }
    }
    value
}

fn coerce(value: Value, schema: &Value) -> Value {
    let mut value = value;
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for nested in all_of {
            value = coerce(value, nested);
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(key).and_then(Value::as_array) {
            value = coerce_union(value, variants);
        }
    }
    let types = schema_types(schema);
    let matches_member = types.len() > 1 && types.iter().any(|kind| matches_type(&value, kind));
    if !types.is_empty() && !matches_member {
        for kind in &types {
            if let Some(candidate) = coerce_primitive(&value, kind)
                && candidate != value
            {
                value = candidate;
                break;
            }
        }
    }
    if types.contains(&"object")
        && let Value::Object(object) = &mut value
    {
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, property_schema) in properties.into_iter().flatten() {
            if let Some(property) = object.get_mut(key) {
                *property = coerce(std::mem::take(property), property_schema);
            }
        }
        if let Some(additional) = schema.get("additionalProperties").filter(|v| v.is_object()) {
            for (key, property) in object.iter_mut() {
                if !properties.is_some_and(|properties| properties.contains_key(key)) {
                    *property = coerce(std::mem::take(property), additional);
                }
            }
        }
    }
    if types.contains(&"array")
        && let Value::Array(items) = &mut value
    {
        for (item, item_schema) in items.iter_mut().zip(item_schemas(schema)) {
            *item = coerce(std::mem::take(item), item_schema);
        }
    }
    value
}

/// Drops `null` for optional properties whose schema does not allow null, as
/// strict-mode providers send them.
fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    match value {
        Value::Array(items) => {
            for (item, item_schema) in items.iter_mut().zip(item_schemas(schema)) {
                normalize_optional_nulls(item, item_schema);
            }
        }
        Value::Object(object) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            let required = required_keys(schema);
            for (key, property_schema) in properties {
                let Some(property) = object.get_mut(key) else {
                    continue;
                };
                if property.is_null()
                    && !required.contains(&key.as_str())
                    && property_schema.get("$ref").is_none()
                    && !is_valid(&Value::Null, property_schema)
                {
                    object.shift_remove(key);
                } else {
                    normalize_optional_nulls(property, property_schema);
                }
            }
        }
        _ => {}
    }
}

fn required_keys(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "root".to_owned()
    } else {
        path.to_owned()
    }
}

fn js_number(value: &Value) -> String {
    yapi_types::json::stringify(value)
}

/// Appends `(path, message)` for every violation of `schema` by `value`.
fn check(value: &Value, schema: &Value, path: &str, errors: &mut Vec<(String, String)>) {
    let Some(schema_object) = schema.as_object() else {
        if schema == &Value::Bool(false) {
            errors.push((display_path(path), "schema is false".into()));
        }
        return;
    };
    let types = schema_types(schema);
    if !types.is_empty() && !types.iter().any(|kind| matches_type(value, kind)) {
        let message = if types.len() == 1 {
            format!("must be {}", types[0])
        } else {
            format!("must be either {}", types.join(" or "))
        };
        errors.push((display_path(path), message));
        return;
    }
    if let Some(constant) = schema_object.get("const")
        && constant != value
    {
        errors.push((display_path(path), "must be equal to constant".into()));
    }
    if let Some(options) = schema_object.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        errors.push((
            display_path(path),
            "must be equal to one of the allowed values".into(),
        ));
    }
    if let Some(variants) = schema_object.get("anyOf").and_then(Value::as_array)
        && !variants.iter().any(|variant| is_valid(value, variant))
    {
        errors.push((display_path(path), "must match a schema in anyOf".into()));
    }
    if let Some(variants) = schema_object.get("oneOf").and_then(Value::as_array)
        && variants
            .iter()
            .filter(|variant| is_valid(value, variant))
            .count()
            != 1
    {
        errors.push((
            display_path(path),
            "must match exactly one schema in oneOf".into(),
        ));
    }
    if let Some(all_of) = schema_object.get("allOf").and_then(Value::as_array) {
        for nested in all_of {
            check(value, nested, path, errors);
        }
    }
    match value {
        Value::Number(n) => {
            let n = n.as_f64().unwrap_or_default();
            let limit = |key: &str| schema_object.get(key).and_then(Value::as_f64);
            let bound =
                |key: &str, comparison: &str, ok: bool, errors: &mut Vec<(String, String)>| {
                    if !ok {
                        errors.push((
                            display_path(path),
                            format!("must be {comparison} {}", js_number(&schema_object[key])),
                        ));
                    }
                };
            if let Some(min) = limit("minimum") {
                bound("minimum", ">=", n >= min, errors);
            }
            if let Some(max) = limit("maximum") {
                bound("maximum", "<=", n <= max, errors);
            }
            if let Some(min) = limit("exclusiveMinimum") {
                bound("exclusiveMinimum", ">", n > min, errors);
            }
            if let Some(max) = limit("exclusiveMaximum") {
                bound("exclusiveMaximum", "<", n < max, errors);
            }
        }
        Value::String(text) => {
            let length = text.chars().count() as u64;
            if let Some(min) = schema_object.get("minLength").and_then(Value::as_u64)
                && length < min
            {
                errors.push((
                    display_path(path),
                    format!("must not have fewer than {min} characters"),
                ));
            }
            if let Some(max) = schema_object.get("maxLength").and_then(Value::as_u64)
                && length > max
            {
                errors.push((
                    display_path(path),
                    format!("must not have more than {max} characters"),
                ));
            }
        }
        Value::Array(items) => {
            if let Some(min) = schema_object.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < min
            {
                errors.push((
                    display_path(path),
                    format!("must not have fewer than {min} items"),
                ));
            }
            if let Some(max) = schema_object.get("maxItems").and_then(Value::as_u64)
                && (items.len() as u64) > max
            {
                errors.push((
                    display_path(path),
                    format!("must not have more than {max} items"),
                ));
            }
            for (index, (item, item_schema)) in items.iter().zip(item_schemas(schema)).enumerate() {
                check(
                    item,
                    item_schema,
                    &join_path(path, &index.to_string()),
                    errors,
                );
            }
        }
        Value::Object(object) => check_object(object, schema_object, path, errors),
        _ => {}
    }
}

fn check_object(
    object: &Map<String, Value>,
    schema: &Map<String, Value>,
    path: &str,
    errors: &mut Vec<(String, String)>,
) {
    let missing: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|key| !object.contains_key(*key))
        .collect();
    if let Some(first) = missing.first() {
        errors.push((
            join_path(path, first),
            format!("must have required properties {}", missing.join(", ")),
        ));
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    for (key, property_schema) in properties.into_iter().flatten() {
        if let Some(property) = object.get(key) {
            check(property, property_schema, &join_path(path, key), errors);
        }
    }
    let extra = object
        .iter()
        .filter(|(key, _)| !properties.is_some_and(|properties| properties.contains_key(*key)));
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            if extra.count() > 0 {
                errors.push((
                    display_path(path),
                    "must not have additional properties".into(),
                ));
            }
        }
        Some(additional @ Value::Object(_)) => {
            for (key, property) in extra {
                check(property, additional, &join_path(path, key), errors);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(parameters: Value) -> ToolDeclaration {
        ToolDeclaration {
            name: "read".into(),
            description: String::new(),
            parameters,
            constrained_sampling: None,
        }
    }

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: arguments.as_object().cloned().unwrap(),
            thought_signature: None,
            namespace: None,
        }
    }

    fn read_schema() -> Value {
        json!({"type":"object","required":["path"],"properties":{
            "path":{"type":"string"},"offset":{"type":"number"},"limit":{"type":"number"}}})
    }

    #[test]
    fn coerces_and_drops_strict_nulls() {
        let args = validate_tool_arguments(
            &tool(read_schema()),
            &call(json!({"path": "a.txt", "offset": "5", "limit": null})),
        )
        .unwrap();
        assert_eq!(args, json!({"path": "a.txt", "offset": 5}));
    }

    #[test]
    fn reports_typebox_style_errors() {
        let err = validate_tool_arguments(&tool(read_schema()), &call(json!({"offset": [1]})))
            .unwrap_err();
        assert_eq!(
            err,
            "Validation failed for tool \"read\":\n  - path: must have required properties path\n  - offset: must be number\n\nReceived arguments:\n{\n  \"offset\": [\n    1\n  ]\n}"
        );
    }
    #[test]
    fn coerces_tuple_and_list_items() {
        let schema = json!({"type":"object","properties":{
            "pair":{"type":"array","items":[{"type":"number"},{"type":"string"}]},
            "list":{"type":"array","items":{"type":"number"}}}});
        let args = validate_tool_arguments(
            &tool(schema),
            &call(json!({"pair": ["1", 2, "x"], "list": ["3", "4"]})),
        )
        .unwrap();
        assert_eq!(args, json!({"pair": [1, "2", "x"], "list": [3, 4]}));
    }
}
