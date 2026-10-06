//! `settings.json`, global and project scope.
//!
//! Mirrors the `Settings` interface in `packages/coding-agent/src/core/settings-manager.ts`
//! in pi `v1.0.0`. Every field is optional; defaults belong to the code that reads them.
//! pi edits this file as a document and keeps unknown keys, so writes go through the
//! document, not through [`Settings`].
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::message::ThinkingLevel;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changelog_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<ThinkingLevel>,
    /// Keyed by `provider/modelId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_thinking_levels: Option<IndexMap<String, ThinkingLevel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_summary: Option<BranchSummarySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_thinking_block: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_cache_miss_notices: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_startup: Option<BoolOr<Header>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_trust: Option<DefaultProjectTrust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command_prefix: Option<String>,
    /// Command for npm operations, in argv form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapse_changelog: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_install_telemetry: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_analytics: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<PackageSource>>,
    /// Paths or patterns; `!`, `+` and `-` prefixes filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_skill_commands: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    /// Tool names; `+name` and `-name` adjust the inherited list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_escape_action: Option<DoubleEscapeAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_filter_mode: Option<TreeFilterMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// 0 to 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_padding_x: Option<u8>,
    /// 0 or 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pad: Option<u8>,
    /// 3 to 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocomplete_max_visible: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_hardware_cursor: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<WarningSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codemode: Option<CodemodeSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<String>,
    /// Milliseconds; pi also accepts a numeric string or `"disabled"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_ms: Option<NumberOr<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_warming: Option<CacheWarming>,
    /// Milliseconds; pi also accepts a numeric string or `"disabled"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<NumberOr<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_mode: Option<TuiMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_exit_output: Option<FullscreenExitOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_scrollbar: Option<Scrollbar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_copy_on_select: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_wheel_scroll_lines: Option<NumberOr<Auto>>,
}

impl Settings {
    /// pi's `getImageAutoResize`: `images.autoResize`, on when unset.
    pub fn image_auto_resize(&self) -> bool {
        self.images
            .as_ref()
            .and_then(|images| images.auto_resize)
            .unwrap_or(true)
    }
}

/// A boolean or a specific alternative value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BoolOr<T> {
    Bool(bool),
    Other(T),
}

/// A number or a specific alternative value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NumberOr<T> {
    Number(u64),
    Other(T),
}

/// The literal `"auto"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auto {
    Auto,
}

/// The literal `"header"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Header {
    Header,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    OneAtATime,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    /// Keyed by `provider/modelId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<IndexMap<String, CompactionModelOverride>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummarySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_prompt: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRetrySettings>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetrySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DefaultProjectTrust {
    Ask,
    Always,
    Never,
}

/// A package to load: a source string, or a source with resource filters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    /// `npm:`, `git:`, a URL, or a local path.
    Source(String),
    Filtered(FilteredPackage),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilteredPackage {
    pub source: String,
    /// `false` loads only resources selected by `+` patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_width_cells: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_on_shrink: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_terminal_progress: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlinks: Option<BoolOr<Auto>>,
    /// `false` disables images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<BoolOr<ImageProtocol>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_color: Option<BoolOr<Auto>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
    Auto,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_images: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoubleEscapeAction {
    Fork,
    Tree,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TreeFilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

/// Thinking token budgets for providers that take a budget instead of a level.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_block_indent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<MermaidMode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MermaidMode {
    Off,
    Final,
    Streaming,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_extra_usage: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CodemodeMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_budget: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeMode {
    On,
    Only,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarming {
    Off,
    Streaming,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TuiMode {
    Regular,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FullscreenExitOutput {
    Transcript,
    ResumeHint,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scrollbar {
    Hidden,
    /// pi's default.
    #[default]
    Auto,
    Always,
}
