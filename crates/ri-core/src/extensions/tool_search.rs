//! The `tool_search` tool: a BM25 ranker over the metadata of tools that are
//! not declared to the model (`codemode` and `deferred` exposure), which
//! activates the matches for the next model call. Port of
//! `extensions/tool-search` in pi `v1.0.0`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use futures_util::future::BoxFuture;
use regex_lite::Regex;
use ri_agent::{Tool, UpdateSink};
use ri_types::event::ToolResult;
use ri_types::message::ToolDeclaration;
use ri_types::rpc::SourceInfo;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Extension, ToolInfo, Tools, builtin_source};
use crate::tools::{Exposure, Namespace, RegisteredTool};

/// The tool's name.
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
/// Matches loaded when the call gives no limit.
pub const DEFAULT_LIMIT: usize = 8;

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// The description, which does not list the searchable tools so it stays the
/// same while tools are registered.
pub const DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

static LOWER_UPPER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new("([a-z0-9])([A-Z])").ok());
static ACRONYM: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new("([A-Z]+)([A-Z][a-z])").ok());
static PLURAL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new("(ches|shes|sses|xes|zes)$").ok());

/// Naive singular form, so `issues` matches `issue`.
fn stem(term: &str) -> String {
    let length = term.chars().count();
    if length > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..term.len() - 3]);
    }
    if length > 4 && PLURAL.as_ref().is_some_and(|plural| plural.is_match(term)) {
        return term[..term.len() - 2].to_owned();
    }
    if length > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..term.len() - 1].to_owned();
    }
    term.to_owned()
}

/// Lowercase terms split at camelCase boundaries and non-alphanumerics,
/// without stop words.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut text = text.to_owned();
    if let Some(regex) = LOWER_UPPER.as_ref() {
        text = regex.replace_all(&text, "$1 $2").into_owned();
    }
    if let Some(regex) = ACRONYM.as_ref() {
        text = regex.replace_all(&text, "$1 $2").into_owned();
    }
    text.to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

/// Schema descriptions and property names, recursively.
fn schema_text(schema: &Value, parts: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(description) = object.get("description").and_then(Value::as_str) {
        parts.push(description.to_owned());
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, parts);
        }
    }
    if let Some(items) = object.get("items") {
        schema_text(items, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for variant in object
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            schema_text(variant, parts);
        }
    }
}

/// A tool's search text: its name, the name with `_` as spaces, the
/// description, schema text, and its namespace.
pub fn document(tool: &ToolInfo) -> String {
    let mut parts = vec![
        tool.name.clone(),
        tool.name.replace('_', " "),
        tool.description.clone(),
    ];
    schema_text(&tool.parameters, &mut parts);
    if let Some(Namespace {
        name,
        description,
        instructions,
    }) = &tool.namespace
    {
        parts.push(name.clone());
        parts.push(description.clone().unwrap_or_default());
        parts.push(instructions.clone().unwrap_or_default());
    }
    parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Okapi BM25 (k1 1.2, b 0.75) over `documents`; the best `limit` names with a
/// positive score. Ties keep document order.
pub fn rank(query: &str, documents: &[(String, String)], limit: usize) -> Vec<(String, f64)> {
    let mut terms = Vec::new();
    for term in tokenize(query) {
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    if terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }
    let (k1, b) = (1.2, 0.75);
    let counts: Vec<HashMap<String, usize>> = documents
        .iter()
        .map(|(_, text)| {
            let mut counts = HashMap::new();
            for term in tokenize(text) {
                *counts.entry(term).or_insert(0) += 1;
            }
            counts
        })
        .collect();
    let lengths: Vec<f64> = counts
        .iter()
        .map(|counts| counts.values().sum::<usize>() as f64)
        .collect();
    let average = lengths.iter().sum::<f64>() / documents.len() as f64;
    let average = if average == 0.0 { 1.0 } else { average };
    let total = documents.len() as f64;
    let idf: HashMap<&str, f64> = terms
        .iter()
        .map(|term| {
            let frequency = counts
                .iter()
                .filter(|counts| counts.contains_key(term))
                .count() as f64;
            (
                term.as_str(),
                (1.0 + (total - frequency + 0.5) / (frequency + 0.5)).ln(),
            )
        })
        .collect();
    let mut matches: Vec<(String, f64)> = Vec::new();
    for (index, (name, _)) in documents.iter().enumerate() {
        let mut score = 0.0;
        for term in &terms {
            let Some(&count) = counts[index].get(term) else {
                continue;
            };
            let count = count as f64;
            let norm = k1 * (1.0 - b + b * lengths[index] / average);
            score += idf.get(term.as_str()).copied().unwrap_or(0.0) * (count * (k1 + 1.0))
                / (count + norm);
        }
        if score > 0.0 {
            matches.push((name.clone(), score));
        }
    }
    matches.sort_by(|a, b| b.1.total_cmp(&a.1));
    matches.truncate(limit);
    matches
}

/// The `tool_search` tool over a session's tools.
pub struct ToolSearch {
    declaration: ToolDeclaration,
    tools: Tools,
}

impl ToolSearch {
    /// The tool, searching `tools`.
    pub fn new(tools: Tools) -> ToolSearch {
        ToolSearch {
            declaration: ToolDeclaration {
                name: TOOL_SEARCH_TOOL_NAME.into(),
                description: DESCRIPTION.into(),
                parameters: json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {"type": "string", "description": "Search query for deferred tools."},
                        "limit": {"type": "number", "description": format!("Maximum number of tools to return. Defaults to {DEFAULT_LIMIT}.")},
                    },
                }),
                constrained_sampling: None,
            },
            tools,
        }
    }

    /// Ranks the searchable tools that are not active and activates the matches.
    fn search_and_load(&self, query: &str, limit: usize) -> Vec<(String, String)> {
        let active = self.tools.active();
        let candidates: Vec<ToolInfo> = self
            .tools
            .all()
            .into_iter()
            .filter(|tool| {
                matches!(tool.exposure, Exposure::Codemode | Exposure::Deferred)
                    && !active.contains(&tool.name)
            })
            .collect();
        let documents: Vec<(String, String)> = candidates
            .iter()
            .map(|tool| (tool.name.clone(), document(tool)))
            .collect();
        let matches = rank(query, &documents, limit);
        if !matches.is_empty() {
            let mut next = active;
            next.extend(matches.iter().map(|(name, _)| name.clone()));
            self.tools.set_active(next);
        }
        matches
            .into_iter()
            .map(|(name, _)| {
                let description = candidates
                    .iter()
                    .find(|tool| tool.name == name)
                    .map(|tool| tool.description.clone())
                    .unwrap_or_default();
                (name, description)
            })
            .collect()
    }
}

impl Tool for ToolSearch {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        _cancel: CancellationToken,
        _updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let query = args["query"].as_str().unwrap_or_default();
            if query.trim().is_empty() {
                return Err("query must not be empty".into());
            }
            let limit = match args.get("limit").and_then(Value::as_f64) {
                None => DEFAULT_LIMIT,
                Some(limit) if limit.fract() == 0.0 && limit > 0.0 => limit as usize,
                Some(_) => return Err("limit must be a positive integer".into()),
            };
            let loaded = self.search_and_load(query, limit);
            let text = if loaded.is_empty() {
                "No matching tools found.".to_owned()
            } else {
                let lines: Vec<String> = loaded
                    .iter()
                    .map(|(name, description)| {
                        let first = description.trim().lines().next().unwrap_or_default();
                        format!("- {name}: {first}")
                    })
                    .collect();
                format!(
                    "Loaded {} tool{}. They are available from your next call:\n{}",
                    loaded.len(),
                    if loaded.len() == 1 { "" } else { "s" },
                    lines.join("\n")
                )
            };
            let names: Vec<&str> = loaded.iter().map(|(name, _)| name.as_str()).collect();
            Ok(ToolResult {
                content: vec![crate::mcp::content::text(text)],
                details: Some(json!({ "loaded": names })),
                ..ToolResult::default()
            })
        })
    }
}

/// The built-in extension that registers `tool_search`, inactive until named
/// by `--tools`, `defaultTools` or another extension.
pub struct ToolSearchExtension;

impl Extension for ToolSearchExtension {
    fn source(&self) -> SourceInfo {
        builtin_source("tool-search")
    }

    fn load(&self, tools: &Tools) {
        tools.register(RegisteredTool {
            tool: Arc::new(ToolSearch::new(tools.clone())),
            snippet: Some("Search for tools that are not loaded yet and load the matches".into()),
            guidelines: Vec::new(),
            exposure: Exposure::ModelOnly,
            namespace: None,
            default_active: false,
        });
    }
}

/// Whether `tool` is this crate's `tool_search`.
pub fn is_tool_search(tool: &ToolInfo) -> bool {
    tool.name == TOOL_SEARCH_TOOL_NAME && tool.exposure == Exposure::ModelOnly
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_like_pi() {
        assert_eq!(
            tokenize("searchIssues in GitHubAPI for the HTTPServer"),
            ["search", "issue", "git", "hub", "api", "http", "server"]
        );
        assert_eq!(tokenize("watches boxes classes"), ["watch", "box", "class"]);
        assert!(tokenize("the and of").is_empty());
    }

    #[test]
    fn ranks_matching_tools_first() {
        let documents = vec![
            (
                "mcp__gh__issues".to_owned(),
                "mcp__gh__issues mcp gh issues List issues of a repository".to_owned(),
            ),
            (
                "mcp__fs__read".to_owned(),
                "mcp__fs__read mcp fs read Read a file".to_owned(),
            ),
        ];
        let ranked = rank("find repository issues", &documents, 8);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, "mcp__gh__issues");
        assert!(rank("", &documents, 8).is_empty());
    }
}
