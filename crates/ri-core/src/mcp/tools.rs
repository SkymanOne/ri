//! MCP tools as session tools, and the three resource tools. Port of
//! `extensions/mcp/tools.ts` and `resources.ts` in pi `v1.0.0`.
//!
//! Model-facing text over 20 KB keeps its start and end, with the full text
//! saved to a temp file the model can read. Binary resources other than
//! images are saved to temp files too. Results keep the server's whole
//! `CallToolResult`, without `_meta`, as structured content.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use futures_util::future::BoxFuture;
use ri_agent::{Tool, UpdateSink};
use ri_types::event::ToolResult;
use ri_types::message::{ContentBlock, ToolDeclaration};
use serde_json::{Map, Value, json};
use sha2::Digest as _;
use tokio_util::sync::CancellationToken;

use super::client::RequestOptions;
use super::connection::Connection;
use super::content::{block_to_llm, text, to_llm_content};
use crate::tools::truncate::{format_size, truncate_middle};

/// Provider tool names hold at most 64 characters of `[A-Za-z0-9_-]`.
const MAX_TOOL_NAME_LENGTH: usize = 64;
/// Model-facing text beyond this is cut in the middle.
pub const OUTPUT_MAX_BYTES: usize = 20 * 1024;
/// Reads the resources resource links name.
pub const READ_MCP_RESOURCE_TOOL: &str = "read_mcp_resource";
/// Lists resources.
pub const LIST_MCP_RESOURCES_TOOL: &str = "list_mcp_resources";
/// Lists resource templates.
pub const LIST_MCP_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";

/// `mcp__<server>__<tool>` with everything but `[A-Za-z0-9_]` as `_`; too
/// long, or taken by another tool, it is cut and given a hash suffix.
pub fn tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name: String = format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    let digest = sha2::Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let hash: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "{}_{hash}",
        &name[..name.len().min(MAX_TOOL_NAME_LENGTH - hash.len() - 1)]
    )
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let _ = getrandom::fill(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Saves `data` to a temp file only the user can read; its path.
async fn save_temp(data: &[u8], extension: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!("ri-mcp-{}{extension}", random_hex(8)));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .await
        .map_err(|error| error.to_string())?;
    tokio::io::AsyncWriteExt::write_all(&mut file, data)
        .await
        .map_err(|error| error.to_string())?;
    Ok(path)
}

fn text_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keeps model-facing text within [`OUTPUT_MAX_BYTES`]: longer text becomes
/// one block in Codex's format with the full text's path; images follow it.
pub async fn limit_content(content: Vec<ContentBlock>) -> (Vec<ContentBlock>, Option<PathBuf>) {
    let combined = text_of(&content);
    let truncation = truncate_middle(&combined, OUTPUT_MAX_BYTES);
    if !truncation.truncated {
        return (content, None);
    }
    let (where_, path) = match save_temp(combined.as_bytes(), ".txt").await {
        Ok(path) => (
            format!(
                "[Full output: {} (read it with offset/limit)]",
                path.display()
            ),
            Some(path),
        ),
        Err(error) => (format!("[Could not save the full output: {error}]"), None),
    };
    let tokens = truncation.total_bytes.div_ceil(4);
    let mut limited = vec![text(format!(
        "Warning: truncated output (original token count: {tokens})\nTotal output lines: {}\n\n{}\n\n{where_}",
        truncation.total_lines, truncation.content
    ))];
    limited.extend(
        content
            .into_iter()
            .filter(|block| matches!(block, ContentBlock::Image(_))),
    );
    (limited, path)
}

/// The extension a saved binary resource gets: its URI's, else `.bin`.
fn extension_of(uri: &str) -> String {
    let path = reqwest::Url::parse(uri)
        .map(|url| url.path().to_owned())
        .unwrap_or_else(|_| uri.to_owned());
    let Some(dot) = path.rfind('.') else {
        return ".bin".into();
    };
    let suffix = &path[dot + 1..];
    if (1..=8).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
        format!(".{suffix}")
    } else {
        ".bin".into()
    }
}

/// Blobs of these types are shown as text.
fn is_text_mime(mime: Option<&str>) -> bool {
    let Some(mime) = mime else {
        return false;
    };
    let kind = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    kind.starts_with("text/")
        || kind == "application/json"
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// One block of `server`'s result for the model.
async fn block_content(server: &str, block: &Value, readable_resources: bool) -> ContentBlock {
    match field(block, "type") {
        Some("resource_link") => {
            let mut details = Vec::new();
            if let Some(mime) = field(block, "mimeType").filter(|mime| !mime.is_empty()) {
                details.push(mime.to_owned());
            }
            if let Some(size) = block.get("size").and_then(Value::as_f64) {
                details.push(format_size(size as usize));
            }
            let details = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join(", "))
            };
            let description = field(block, "description")
                .filter(|text| !text.is_empty())
                .map(|text| format!(": {text}"))
                .unwrap_or_default();
            let read = if readable_resources {
                format!(". Read it with {READ_MCP_RESOURCE_TOOL} (server \"{server}\")")
            } else {
                String::new()
            };
            let title = field(block, "title")
                .or(field(block, "name"))
                .unwrap_or_default();
            text(format!(
                "[Resource {} \"{title}\"{details}{description}{read}]",
                field(block, "uri").unwrap_or_default()
            ))
        }
        Some("resource")
            if block["resource"].get("blob").is_some()
                && !field(&block["resource"], "mimeType")
                    .is_some_and(|mime| mime.starts_with("image/")) =>
        {
            let resource = &block["resource"];
            let uri = field(resource, "uri").unwrap_or_default();
            let mime = field(resource, "mimeType");
            let data = base64::engine::general_purpose::STANDARD
                .decode(field(resource, "blob").unwrap_or_default())
                .unwrap_or_default();
            if is_text_mime(mime) {
                return text(String::from_utf8_lossy(&data));
            }
            let kind = format!(
                "{}, {}",
                mime.unwrap_or("unknown type"),
                format_size(data.len())
            );
            match save_temp(&data, &extension_of(uri)).await {
                Ok(path) => text(format!(
                    "[Binary resource {uri} ({kind}) saved to {}]",
                    path.display()
                )),
                Err(error) => text(format!(
                    "[Binary resource {uri} ({kind}) could not be saved: {error}]"
                )),
            }
        }
        _ => block_to_llm(block),
    }
}

/// `server`'s content blocks for the model, before the output limit.
pub async fn model_content(
    server: &str,
    blocks: &[Value],
    readable_resources: bool,
) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    for block in blocks {
        content.push(block_content(server, block, readable_resources).await);
    }
    content
}

/// pi's `convertMcpResult`: `isError` results become error results.
pub async fn convert_result(
    server: &str,
    tool: &str,
    result: Value,
    readable_resources: bool,
) -> ToolResult {
    let blocks = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut converted = if blocks.is_empty() {
        to_llm_content(&result)
    } else {
        model_content(server, &blocks, readable_resources).await
    };
    let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
    if is_error && text_of(&converted).is_empty() {
        converted.push(text(format!("MCP tool {server}/{tool} returned an error")));
    }
    let (content, path) = limit_content(converted).await;
    let mut details = json!({"server": server, "tool": tool});
    if let Some(path) = path {
        details["fullOutputPath"] = json!(path.display().to_string());
    }
    let mut structured = result;
    if let Some(object) = structured.as_object_mut() {
        object.shift_remove("_meta");
    }
    ToolResult {
        content,
        details: Some(details),
        structured_content: Some(structured),
        is_error: is_error.then_some(true),
        ..ToolResult::default()
    }
}

/// Tool input schemas must be objects with `properties`, which some servers
/// omit.
fn parameters(schema: &Map<String, Value>) -> Value {
    let mut parameters = schema.clone();
    if !parameters.contains_key("type") {
        parameters.insert("type".into(), json!("object"));
    }
    if !parameters.contains_key("properties") {
        parameters.insert("properties".into(), json!({}));
    }
    Value::Object(parameters)
}

/// An MCP server's tool.
pub struct McpTool {
    declaration: ToolDeclaration,
    output_schema: Value,
    label: String,
    server: String,
    tool: String,
    connection: Arc<Connection>,
}

impl McpTool {
    /// The tool `tool` of `connection`'s server, named `name`.
    pub fn new(connection: Arc<Connection>, tool: &super::Tool, name: String) -> McpTool {
        let server = connection.name().to_owned();
        let title = tool.title.clone().or_else(|| {
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.get("title"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let description = tool
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .or(title)
            .unwrap_or_else(|| format!("MCP tool {} from server {server}", tool.name));
        McpTool {
            declaration: ToolDeclaration {
                name,
                description,
                parameters: parameters(&tool.input_schema),
                constrained_sampling: None,
            },
            output_schema: result_schema(tool.output_schema.as_ref()),
            label: format!("{server}/{}", tool.name),
            server,
            tool: tool.name.clone(),
            connection,
        }
    }
}

/// pi's `createMcpResultSchema`: the `CallToolResult` scripts receive, with
/// the tool's own output schema as `structuredContent`.
fn result_schema(structured: Option<&Map<String, Value>>) -> Value {
    let mut properties = Map::new();
    properties.insert(
        "content".into(),
        json!({"type": "array", "items": {"type": "object"}}),
    );
    if let Some(structured) = structured {
        properties.insert(
            "structuredContent".into(),
            Value::Object(structured.clone()),
        );
    }
    properties.insert("isError".into(), json!({"type": "boolean"}));
    properties.insert("_meta".into(), json!({"type": "object"}));
    json!({"type": "object", "properties": properties, "required": ["content"]})
}

impl Tool for McpTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&self.output_schema)
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        cancel: CancellationToken,
        updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let (server, tool) = (self.server.clone(), self.tool.clone());
            let options = RequestOptions {
                cancel: Some(cancel),
                timeout: Some(self.connection.timeout()),
                on_progress: Some(Arc::new(move |progress: &Value| {
                    let total = progress
                        .get("total")
                        .filter(|total| !total.is_null())
                        .map(|total| format!("/{total}"))
                        .unwrap_or_default();
                    let message = progress
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("Progress {}{total}", progress["progress"]));
                    updates(ToolResult {
                        content: vec![text(message)],
                        details: Some(json!({"server": server, "tool": tool})),
                        ..ToolResult::default()
                    });
                })),
            };
            let args = if args.is_null() { json!({}) } else { args };
            let result = self
                .connection
                .call_tool(&self.tool, args, options)
                .await
                .map_err(|error| error.to_string())?;
            Ok(convert_result(
                &self.server,
                &self.tool,
                result,
                self.connection.readable_resources(),
            )
            .await)
        })
    }
}

/// Which resource tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourceTool {
    List,
    ListTemplates,
    Read,
}

/// The servers resource tools reach, at call time.
pub type ResourceServers = Arc<dyn Fn() -> Vec<Arc<Connection>> + Send + Sync>;

/// One of `list_mcp_resources`, `list_mcp_resource_templates` and
/// `read_mcp_resource`, which take a `server` argument and reach every
/// connected server with resources.
pub struct McpResourceTool {
    declaration: ToolDeclaration,
    kind: ResourceTool,
    servers: ResourceServers,
}

/// MCP App user interfaces, which only hosts that render them can use.
pub fn is_app_resource(item: &Value) -> bool {
    let uri = field(item, "uri")
        .or_else(|| field(item, "uriTemplate"))
        .unwrap_or_default();
    let mime = field(item, "mimeType")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let profile = mime.split(';').skip(1).any(|parameter| {
        parameter.split_once('=').is_some_and(|(key, value)| {
            key.trim() == "profile" && value.trim().trim_matches('"') == "mcp-app"
        })
    });
    uri.starts_with("ui://") || profile
}

/// A listed item without `_meta` and icons, tagged with its server.
fn listed(server: &str, item: &Value) -> Value {
    let mut out = Map::new();
    out.insert("server".into(), json!(server));
    if let Some(object) = item.as_object() {
        for (key, value) in object {
            if key != "_meta" && key != "icons" {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(out)
}

fn string_argument(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            Ok(Some(value.trim().to_owned()).filter(|value| !value.is_empty()))
        }
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// The three resource tools over `servers`.
pub fn resource_tools(servers: ResourceServers) -> Vec<McpResourceTool> {
    let string = |description: &str| json!({"type": "string", "description": description});
    let list_parameters = json!({
        "type": "object",
        "properties": {
            "server": string("MCP server name. Omit to list every server with resources."),
            "cursor": string("Opaque cursor from a previous call with the same server; omit for the first page."),
        },
        "additionalProperties": false,
    });
    let read_parameters = json!({
        "type": "object",
        "properties": {
            "server": string("MCP server name exactly as configured. Must match the 'server' field returned by list_mcp_resources."),
            "uri": string("Resource URI to read. Must be one of the URIs returned by list_mcp_resources."),
        },
        "required": ["server", "uri"],
        "additionalProperties": false,
    });
    let declare = |name: &str, description: &str, parameters: Value| ToolDeclaration {
        name: name.into(),
        description: description.into(),
        parameters,
        constrained_sampling: None,
    };
    vec![
        McpResourceTool {
            declaration: declare(
                LIST_MCP_RESOURCES_TOOL,
                "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models, such as files, database schemas, or application-specific information. Prefer resources over web search when possible.",
                list_parameters.clone(),
            ),
            kind: ResourceTool::List,
            servers: Arc::clone(&servers),
        },
        McpResourceTool {
            declaration: declare(
                LIST_MCP_RESOURCE_TEMPLATES_TOOL,
                "Lists resource templates provided by MCP servers. Parameterized resource templates allow servers to share data that takes parameters and provides context to language models, such as files, database schemas, or application-specific information. Prefer resource templates over web search when possible.",
                list_parameters,
            ),
            kind: ResourceTool::ListTemplates,
            servers: Arc::clone(&servers),
        },
        McpResourceTool {
            declaration: declare(
                READ_MCP_RESOURCE_TOOL,
                "Read a specific resource from an MCP server given the server name and resource URI.",
                read_parameters,
            ),
            kind: ResourceTool::Read,
            servers,
        },
    ]
}

impl McpResourceTool {
    fn find(&self, name: &str) -> Result<Arc<Connection>, String> {
        let servers = (self.servers)();
        if let Some(server) = servers.iter().find(|server| server.name() == name) {
            return Ok(Arc::clone(server));
        }
        let available: Vec<&str> = servers.iter().map(|server| server.name()).collect();
        Err(if available.is_empty() {
            format!("MCP server \"{name}\" has no resources")
        } else {
            format!(
                "MCP server \"{name}\" has no resources. Servers with resources: {}",
                available.join(", ")
            )
        })
    }

    async fn list(&self, args: &Value, cancel: &CancellationToken) -> Result<Value, String> {
        let key = if self.kind == ResourceTool::List {
            "resources"
        } else {
            "resourceTemplates"
        };
        let server_name = string_argument(args, "server")?;
        let cursor = string_argument(args, "cursor")?;
        let options = |server: &Connection| RequestOptions {
            cancel: Some(cancel.clone()),
            timeout: Some(server.timeout()),
            on_progress: None,
        };
        if let Some(name) = server_name {
            let server = self.find(&name)?;
            let (items, next) = if self.kind == ResourceTool::List {
                server.resources_page(cursor, options(&server)).await
            } else {
                server
                    .resource_templates_page(cursor, options(&server))
                    .await
            }
            .map_err(|error| error.to_string())?;
            let items: Vec<Value> = items
                .iter()
                .filter(|item| !is_app_resource(item))
                .map(|item| listed(server.name(), item))
                .collect();
            let mut payload = json!({ "server": server.name(), key: items });
            if let Some(next) = next {
                payload["nextCursor"] = json!(next);
            }
            return Ok(payload);
        }
        if cursor.is_some() {
            return Err("cursor can only be used when a server is specified".into());
        }
        let mut servers = (self.servers)();
        servers.sort_by(|a, b| ri_types::collate::locale_compare(a.name(), b.name()));
        let mut items = Vec::new();
        let mut errors = Vec::new();
        for server in &servers {
            let result = if self.kind == ResourceTool::List {
                server.all_resources(options(server)).await
            } else {
                server.all_resource_templates(options(server)).await
            };
            match result {
                Ok(found) => items.extend(
                    found
                        .iter()
                        .filter(|item| !is_app_resource(item))
                        .map(|item| listed(server.name(), item)),
                ),
                Err(error) => {
                    errors.push(json!({"server": server.name(), "error": error.to_string()}))
                }
            }
        }
        let mut payload = json!({ key: items });
        if !errors.is_empty() {
            payload["errors"] = json!(errors);
        }
        Ok(payload)
    }

    async fn read(&self, args: &Value, cancel: &CancellationToken) -> Result<ToolResult, String> {
        let name = string_argument(args, "server")?.ok_or("server must be provided")?;
        let uri = string_argument(args, "uri")?.ok_or("uri must be provided")?;
        let server = self.find(&name)?;
        let result = server
            .read_resource(
                &uri,
                RequestOptions {
                    cancel: Some(cancel.clone()),
                    timeout: Some(server.timeout()),
                    on_progress: None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let contents = result["contents"].as_array().cloned().unwrap_or_default();
        let mut blocks = Vec::new();
        for item in &contents {
            if contents.len() > 1 {
                blocks.push(json!({"type": "text", "text": format!("{}:", field(item, "uri").unwrap_or_default())}));
            }
            blocks.push(json!({"type": "resource", "resource": item}));
        }
        let converted = model_content(server.name(), &blocks, false).await;
        let converted = if converted.is_empty() {
            vec![text(format!("Resource {uri} is empty."))]
        } else {
            converted
        };
        let (content, path) = limit_content(converted).await;
        let mut details = json!({"server": server.name(), "tool": READ_MCP_RESOURCE_TOOL});
        if let Some(path) = path {
            details["fullOutputPath"] = json!(path.display().to_string());
        }
        let contents: Vec<Value> = contents
            .into_iter()
            .map(|mut item| {
                if let Some(object) = item.as_object_mut() {
                    object.shift_remove("_meta");
                }
                item
            })
            .collect();
        Ok(ToolResult {
            content,
            details: Some(details),
            structured_content: Some(
                json!({"server": server.name(), "uri": uri, "contents": contents}),
            ),
            ..ToolResult::default()
        })
    }
}

impl Tool for McpResourceTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        cancel: CancellationToken,
        _updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            if self.kind == ResourceTool::Read {
                return self.read(&args, &cancel).await;
            }
            let payload = self.list(&args, &cancel).await?;
            let json = ri_types::json::to_string(&payload).map_err(|error| error.to_string())?;
            let (content, path) = limit_content(vec![text(json)]).await;
            let server = string_argument(&args, "server")?.unwrap_or_default();
            let mut details = json!({"server": server, "tool": self.declaration.name});
            if let Some(path) = path {
                details["fullOutputPath"] = json!(path.display().to_string());
            }
            Ok(ToolResult {
                content,
                details: Some(details),
                structured_content: Some(payload),
                ..ToolResult::default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_tools_like_pi() {
        assert_eq!(
            tool_name("docs", "search-pages", |_| false),
            "mcp__docs__search_pages"
        );
        let long = tool_name("server", &"x".repeat(80), |_| false);
        assert_eq!(long.len(), 64);
        assert!(long.starts_with("mcp__server__xxx"));
        let taken = tool_name("a", "b", |name| name == "mcp__a__b");
        assert_eq!(&taken[..10], "mcp__a__b_");
        assert_eq!(taken.len(), 18);
        assert_eq!(extension_of("file:///tmp/archive.tar.gz"), ".gz");
        assert_eq!(extension_of("blob:thing"), ".bin");
        assert!(is_app_resource(&json!({"uri": "ui://widget"})));
        assert!(is_app_resource(
            &json!({"uri": "a", "mimeType": "text/html;profile=mcp-app"})
        ));
    }
}
