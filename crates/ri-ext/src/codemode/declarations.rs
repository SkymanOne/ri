//! What scripts and the model read about tools: TypeScript declarations from
//! JSON Schemas and the codemode tool's catalog. Port of `declarations.ts` and
//! `identifier.ts` in `pi-codemode` and of the description parts of
//! `extensions/codemode/tool.ts` in pi-coding-agent, `v1.0.0`.

use std::collections::HashSet;

use ri_core::tools::Namespace;
use serde_json::{Map, Value};

const INDENT: &str = "  ";
/// Largest rendered input type, in UTF-16 units, before it becomes `unknown`.
const INPUT_SCHEMA_MAX_CHARS: usize = 16_000;
/// Local `$ref` expansions per rendered schema.
const MAX_REF_EXPANSIONS: usize = 32;
/// Characters per token when estimating the cost of a tool section.
const CHARS_PER_TOKEN: usize = 4;
/// pi's `DEFAULT_CODEMODE_INLINE_BUDGET`, in estimated tokens.
pub const DEFAULT_INLINE_BUDGET: u64 = 3000;

/// TypeScript types for MCP results, which `CallToolResult<T>` declarations
/// refer to; pi's `MCP_TYPESCRIPT_PREAMBLE`.
const MCP_TYPES: &str = include_str!("mcp-types.ts");

const INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. Calls still running when the script ends are cancelled.
- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// The globals block; [`description`] adds the `models` line.
const GLOBALS: &str = "Globals:
- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script.
- `store(key, value)` and `load(key)` keep JSON values across codemode calls.
- `ALL_TOOLS`, `searchTools(query, { limit?, namespace? })`, `describeTool(name)`, `describeNamespace(name)`: find unlisted tools, such as MCP tools.";

/// What a script sees of a tool.
#[derive(Clone, Debug, PartialEq)]
pub struct Declaration {
    /// The tool's name.
    pub name: String,
    /// Its description.
    pub description: String,
    /// Its argument schema.
    pub input: Value,
    /// What a call resolves to; `{"type": "string"}` for text output.
    pub output: Value,
    /// Its group.
    pub namespace: Option<Namespace>,
}

/// The identifier a script uses for tool `name`; pi's `toCodemodeIdentifier`.
pub fn identifier(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let valid = if out.is_empty() {
            c.is_ascii_alphabetic() || c == '_' || c == '$'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '$'
        };
        out.push(if valid { c } else { '_' });
    }
    if out.is_empty() { "_".into() } else { out }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn stringify(value: &Value) -> String {
    ri_types::json::to_string(value).unwrap_or_else(|_| "unknown".into())
}

fn union(types: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for entry in types {
        if !unique.contains(&entry) {
            unique.push(entry);
        }
    }
    if unique.iter().any(|entry| entry == "unknown") {
        return "unknown".into();
    }
    if unique.is_empty() {
        "never".into()
    } else {
        unique.join(" | ")
    }
}

struct Schemas<'a> {
    root: &'a Value,
    resolving: Vec<String>,
    expansions: usize,
}

fn decode_component(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = segment
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn resolve_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    for segment in reference
        .get(2..)
        .unwrap_or_default()
        .split('/')
        .filter(|segment| !segment.is_empty())
    {
        let key = decode_component(segment)
            .replace("~1", "/")
            .replace("~0", "~");
        current = current.as_object()?.get(&key)?;
    }
    (current.is_boolean() || current.is_object()).then_some(current)
}

impl Schemas<'_> {
    fn type_of(&mut self, schema: &Value) -> String {
        let object = match schema {
            Value::Bool(true) => return "unknown".into(),
            Value::Bool(false) => return "never".into(),
            Value::Object(object) => object,
            _ => return "unknown".into(),
        };
        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            if self.resolving.iter().any(|entry| entry == reference)
                || self.expansions >= MAX_REF_EXPANSIONS
            {
                return "unknown".into();
            }
            let Some(target) = resolve_ref(reference, self.root) else {
                return "unknown".into();
            };
            self.expansions += 1;
            self.resolving.push(reference.to_owned());
            let out = self.type_of(target);
            self.resolving.pop();
            return out;
        }
        if let Some(constant) = object.get("const") {
            return stringify(constant);
        }
        if let Some(Value::Array(values)) = object.get("enum") {
            return union(values.iter().map(stringify).collect());
        }
        let variants = match (object.get("anyOf"), object.get("oneOf")) {
            (Some(Value::Array(variants)), _) | (_, Some(Value::Array(variants))) => Some(variants),
            _ => None,
        };
        if let Some(variants) = variants {
            let types = variants
                .iter()
                .map(|variant| self.type_of(variant))
                .collect();
            return union(types);
        }
        if let Some(Value::Array(parts)) = object.get("allOf") {
            let parts: Vec<String> = parts
                .iter()
                .map(|part| self.type_of(part))
                .filter(|part| part != "unknown")
                .map(|part| {
                    if part.contains(" | ") {
                        format!("({part})")
                    } else {
                        part
                    }
                })
                .collect();
            return if parts.is_empty() {
                "unknown".into()
            } else {
                parts.join(" & ")
            };
        }
        match object.get("type") {
            Some(Value::Array(types)) => {
                let types = types
                    .iter()
                    .map(|entry| {
                        let mut variant = object.clone();
                        variant.insert("type".into(), entry.clone());
                        self.type_of(&Value::Object(variant))
                    })
                    .collect();
                union(types)
            }
            Some(Value::String(kind)) => match kind.as_str() {
                "string" => "string".into(),
                "number" | "integer" => "number".into(),
                "boolean" => "boolean".into(),
                "null" => "null".into(),
                "array" => self.array_type(object),
                "object" => self.object_type(object),
                _ => "unknown".into(),
            },
            None => {
                if ["properties", "additionalProperties", "required"]
                    .iter()
                    .any(|key| object.contains_key(*key))
                {
                    self.object_type(object)
                } else if object.contains_key("items") || object.contains_key("prefixItems") {
                    self.array_type(object)
                } else {
                    "unknown".into()
                }
            }
            Some(_) => "unknown".into(),
        }
    }

    fn array_type(&mut self, schema: &Map<String, Value>) -> String {
        match schema.get("items") {
            Some(items) if !items.is_array() => return format!("Array<{}>", self.type_of(items)),
            _ => {}
        }
        let tuple = match (schema.get("prefixItems"), schema.get("items")) {
            (Some(Value::Array(items)), _) | (_, Some(Value::Array(items))) => items.as_slice(),
            _ => &[],
        };
        if tuple.is_empty() {
            return "unknown[]".into();
        }
        let types: Vec<String> = tuple.iter().map(|item| self.type_of(item)).collect();
        format!("[{}]", types.join(", "))
    }

    fn object_type(&mut self, schema: &Map<String, Value>) -> String {
        let empty = Map::new();
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let required: Vec<&Value> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|required| required.iter().collect())
            .unwrap_or_default();
        let mut names: Vec<&String> = properties.keys().collect();
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let mut members: Vec<String> = names
            .iter()
            .map(|name| {
                let optional = if required.iter().any(|entry| entry.as_str() == Some(name)) {
                    ""
                } else {
                    "?"
                };
                let key = if is_identifier(name) {
                    (*name).clone()
                } else {
                    stringify(&Value::String((*name).clone()))
                };
                format!("{key}{optional}: {};", self.type_of(&properties[*name]))
            })
            .collect();
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {}
            Some(additional) => {
                let kind = if additional == &Value::Bool(true) {
                    "unknown".into()
                } else {
                    self.type_of(additional)
                };
                members.push(format!("[key: string]: {kind};"));
            }
            None if names.is_empty() => members.push("[key: string]: unknown;".into()),
            None => {}
        }
        if members.is_empty() {
            return "{}".into();
        }
        let description = |name: &str| {
            properties[name]
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_owned()
        };
        if !names.iter().any(|name| !description(name).is_empty()) {
            return format!("{{ {} }}", members.join(" "));
        }
        let mut lines = vec!["{".to_owned()];
        for (index, name) in names.iter().enumerate() {
            for line in description(name).split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line).trim();
                if !line.is_empty() {
                    lines.push(format!("{INDENT}// {line}"));
                }
            }
            lines.push(format!(
                "{INDENT}{}",
                members[index].replace('\n', &format!("\n{INDENT}"))
            ));
        }
        for member in &members[names.len()..] {
            lines.push(format!("{INDENT}{member}"));
        }
        lines.push("}".into());
        lines.join("\n")
    }
}

/// The TypeScript type of JSON Schema `schema`; pi's `schemaToType`. Types
/// longer than `max_chars` become `unknown`.
pub fn schema_to_type(schema: &Value, max_chars: Option<usize>) -> String {
    let out = Schemas {
        root: schema,
        resolving: Vec::new(),
        expansions: 0,
    }
    .type_of(schema);
    match max_chars {
        Some(max) if ri_types::js::len(&out) > max => "unknown".into(),
        _ => out,
    }
}

/// The `structuredContent` schema of an MCP `CallToolResult` output schema,
/// `true` when it declares none, or `None` when the schema is not one; pi's
/// `mcpStructuredContentSchema`.
pub fn mcp_structured_content(schema: &Value) -> Option<Value> {
    let properties = schema.get("properties")?.as_object()?;
    let kind = |key: &str| properties.get(key)?.as_object()?.get("type")?.as_str();
    let items_kind = properties
        .get("content")
        .and_then(|content| content.get("items"))
        .filter(|items| items.is_object())
        .and_then(|items| items.get("type"))
        .and_then(Value::as_str);
    if kind("content") != Some("array")
        || items_kind != Some("object")
        || kind("isError") != Some("boolean")
        || kind("_meta") != Some("object")
    {
        return None;
    }
    match properties.get("structuredContent") {
        Some(structured @ (Value::Object(_) | Value::Bool(_))) => Some(structured.clone()),
        _ => Some(Value::Bool(true)),
    }
}

/// The type a call resolves to; pi's `renderToolOutputType`.
pub fn output_type(schema: &Value) -> String {
    if let Some(structured) = mcp_structured_content(schema) {
        let kind = schema_to_type(&structured, None);
        return if kind == "unknown" {
            "CallToolResult".into()
        } else {
            format!("CallToolResult<{kind}>")
        };
    }
    schema_to_type(schema, None)
}

/// The tool's description and declaration; pi's `renderToolSample`.
pub fn sample(declaration: &Declaration) -> String {
    let input = schema_to_type(&declaration.input, Some(INPUT_SCHEMA_MAX_CHARS));
    format!(
        "{}\n\ncodemode tool declaration:\n```ts\ndeclare const tools: {{ {}(args: {input}): Promise<{}>; }};\n```",
        declaration.description.trim(),
        identifier(&declaration.name),
        output_type(&declaration.output)
    )
}

fn section(declaration: &Declaration) -> String {
    let id = identifier(&declaration.name);
    let heading = if id == declaration.name {
        format!("### `{id}`")
    } else {
        format!("### `{id}` (`{}`)", declaration.name)
    };
    format!("{heading}\n{}", sample(declaration).trim())
}

struct Entry {
    name: String,
    section: String,
    cost: usize,
}

struct Group {
    namespace: Option<Namespace>,
    entries: Vec<Entry>,
}

/// The sections that fit `budget`: in each round every group places its
/// cheapest remaining tool, and a group whose next tool does not fit drops
/// out; pi's `selectCatalog`.
fn select(groups: &[Group], budget: Option<u64>) -> HashSet<String> {
    let Some(budget) = budget else {
        return groups
            .iter()
            .flat_map(|group| group.entries.iter().map(|entry| entry.name.clone()))
            .collect();
    };
    let mut queues: Vec<std::collections::VecDeque<&Entry>> = groups
        .iter()
        .map(|group| {
            let mut entries: Vec<&Entry> = group.entries.iter().collect();
            entries.sort_by_key(|entry| entry.cost);
            entries.into()
        })
        .filter(|queue: &std::collections::VecDeque<&Entry>| !queue.is_empty())
        .collect();
    let mut shown = HashSet::new();
    let mut remaining = budget as usize;
    while !queues.is_empty() {
        queues.retain_mut(|queue| {
            let Some(next) = queue.front() else {
                return false;
            };
            if next.cost > remaining {
                return false;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queue.pop_front();
            !queue.is_empty()
        });
    }
    shown
}

/// The codemode tool's description for `listed` tools, of which `deferred`
/// ones are callable but never listed; pi's `createCodemodeDescription`.
pub fn description(
    listed: &[Declaration],
    deferred: &HashSet<String>,
    budget: Option<u64>,
    docs: Option<&str>,
) -> String {
    let declarations: Vec<&Declaration> = listed
        .iter()
        .filter(|declaration| !deferred.contains(&declaration.name))
        .collect();
    let mut groups: Vec<(String, Group)> = vec![(
        String::new(),
        Group {
            namespace: None,
            entries: Vec::new(),
        },
    )];
    for declaration in &declarations {
        let key = declaration
            .namespace
            .as_ref()
            .map_or_else(String::new, |namespace| format!("ns:{}", namespace.name));
        let index = match groups.iter().position(|(existing, _)| *existing == key) {
            Some(index) => index,
            None => {
                groups.push((
                    key,
                    Group {
                        namespace: declaration.namespace.clone(),
                        entries: Vec::new(),
                    },
                ));
                groups.len() - 1
            }
        };
        let text = section(declaration);
        let cost = ri_types::js::len(&text).div_ceil(CHARS_PER_TOKEN);
        groups[index].1.entries.push(Entry {
            name: declaration.name.clone(),
            section: text,
            cost,
        });
    }
    let mut ordered: Vec<Group> = groups.into_iter().map(|(_, group)| group).collect();
    ordered.sort_by(|a, b| match (&a.namespace, &b.namespace) {
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => ri_types::collate::locale_compare(&a.name, &b.name),
    });
    let shown = select(&ordered, budget);

    let globals = match docs {
        Some(docs) => {
            format!("{GLOBALS}\n- `models`: classifiers and image generation. Read {docs} first.")
        }
        None => GLOBALS.to_owned(),
    };
    let mut sections = vec![INTRO.to_owned(), globals];
    if declarations.iter().any(|declaration| {
        shown.contains(&declaration.name) && mcp_structured_content(&declaration.output).is_some()
    }) {
        sections.push(format!("Shared MCP Types:\n```ts\n{MCP_TYPES}\n```"));
    }
    if declarations.is_empty() {
        return sections.join("\n\n");
    }
    let mut tools = vec!["Nested tools:".to_owned()];
    for group in &ordered {
        let visible: Vec<&Entry> = group
            .entries
            .iter()
            .filter(|entry| shown.contains(&entry.name))
            .collect();
        if let Some(namespace) = &group.namespace {
            let listing = if visible.len() == group.entries.len() {
                ""
            } else if visible.is_empty() {
                " (tools not listed)"
            } else {
                " (some tools not listed)"
            };
            let description = namespace
                .description
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| format!("\n{text}"))
                .unwrap_or_default();
            tools.push(format!("## {}{listing}{description}", namespace.name));
        }
        tools.extend(visible.iter().map(|entry| entry.section.clone()));
    }
    sections.push(tools.join("\n\n"));
    sections.join("\n\n")
}

/// What a script call resolves to, in one line; pi's `describeOutput`.
fn describe_output(schema: &Value) -> String {
    let kind = output_type(schema);
    if kind == "string" {
        return "a string".into();
    }
    if schema.get("type").and_then(Value::as_str) == Some("object")
        && let Some(properties) = schema.get("properties").and_then(Value::as_object)
        && mcp_structured_content(schema).is_none()
    {
        let required = schema.get("required").and_then(Value::as_array);
        let fields: Vec<String> = properties
            .keys()
            .map(|name| {
                if required.is_some_and(|required| {
                    required.iter().any(|entry| entry.as_str() == Some(name))
                }) {
                    name.clone()
                } else {
                    format!("{name}?")
                }
            })
            .collect();
        return format!("`{{ {} }}`", fields.join(", "));
    }
    let collapsed = kind.split_whitespace().collect::<Vec<_>>().join(" ");
    // `/\s+/g` keeps leading and trailing runs as one space.
    let lead = if kind.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let trail = if kind.ends_with(char::is_whitespace) && !collapsed.is_empty() {
        " "
    } else {
        ""
    };
    format!("`{lead}{collapsed}{trail}`")
}

/// A declared tool's description followed by how scripts call it; pi's
/// `describeScriptCall`.
pub fn script_call(declaration: &Declaration) -> String {
    format!(
        "{}\n\nCodemode: `tools.{}(args)` resolves to {}.",
        declaration.description.trim(),
        identifier(&declaration.name),
        describe_output(&declaration.output)
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn declaration(name: &str, description: &str, input: Value, output: Value) -> Declaration {
        Declaration {
            name: name.into(),
            description: description.into(),
            input,
            output,
            namespace: None,
        }
    }

    #[test]
    fn identifiers_replace_invalid_characters() {
        assert_eq!(identifier("mcp__docs__fetch-page"), "mcp__docs__fetch_page");
        assert_eq!(identifier("1a"), "_a");
        assert_eq!(identifier(""), "_");
        assert_eq!(identifier("é"), "_");
    }

    #[test]
    fn renders_pi_samples() {
        let bash = declaration(
            "bash",
            "Run.",
            json!({"type":"object","required":["command"],"properties":{
                "command":{"type":"string","description":"Shell command to execute"},
                "timeout":{"type":"number","description":"Timeout in seconds (optional, no default timeout)"}}}),
            json!({"type":"object","required":["output","truncated","exit_code","wall_time_seconds"],"properties":{
                "output":{"type":"string","description":"Combined stdout and stderr, possibly truncated"},
                "truncated":{"type":"boolean"},
                "full_output_path":{"type":"string","description":"Full output, when truncated"},
                "exit_code":{"type":"number"},
                "wall_time_seconds":{"type":"number"}}}),
        );
        assert_eq!(
            sample(&bash),
            "Run.\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { bash(args: {\n  // Shell command to execute\n  command: string;\n  // Timeout in seconds (optional, no default timeout)\n  timeout?: number;\n}): Promise<{\n  exit_code: number;\n  // Full output, when truncated\n  full_output_path?: string;\n  // Combined stdout and stderr, possibly truncated\n  output: string;\n  truncated: boolean;\n  wall_time_seconds: number;\n}>; };\n```"
        );
        assert_eq!(
            script_call(&bash),
            "Run.\n\nCodemode: `tools.bash(args)` resolves to `{ output, truncated, full_output_path?, exit_code, wall_time_seconds }`."
        );
        let helper = declaration(
            "helper",
            "A helper with no namespace.",
            json!({"type":"object","properties":{"x":{"type":"number"}}}),
            json!({"type":"string"}),
        );
        assert!(sample(&helper).ends_with(
            "declare const tools: { helper(args: { x?: number; }): Promise<string>; };\n```"
        ));
        assert_eq!(
            script_call(&helper),
            "A helper with no namespace.\n\nCodemode: `tools.helper(args)` resolves to a string."
        );
    }

    #[test]
    fn types_cover_references_unions_and_tuples() {
        let schema = json!({
            "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
            "type": "object",
            "properties": {
                "a": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "b": {"enum": ["x", 1]},
                "c": {"prefixItems": [{"type": "string"}, {"type": "integer"}]},
                "d": {"$ref": "#/$defs/node"},
                "e-f": {"allOf": [{"type": "object", "properties": {"k": {"const": true}}, "required": ["k"]}, {"type": ["string", "number"]}]},
            },
            "required": ["a"],
            "additionalProperties": false,
        });
        assert_eq!(
            schema_to_type(&schema, None),
            "{ a: string | null; b?: \"x\" | 1; c?: [string, number]; d?: { next?: unknown; }; \"e-f\"?: { k: true; } & (string | number); }"
        );
        assert_eq!(schema_to_type(&json!({}), None), "unknown");
        assert_eq!(
            schema_to_type(&json!({"type": "object"}), None),
            "{ [key: string]: unknown; }"
        );
        assert_eq!(schema_to_type(&json!({"type": "array"}), None), "unknown[]");
    }
}
