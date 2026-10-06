//! pi's JSON configuration files and how pi lays each one out on disk.

use serde::Serialize;

use crate::json;

/// A JSON configuration file in the agent or project directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigFile {
    /// `settings.json`, global and project scope.
    Settings,
    /// `auth.json`, global scope.
    Auth,
    /// `models.json`, global scope. pi reads it but never writes it.
    Models,
    /// `keybindings.json`, global scope.
    Keybindings,
    /// `mcp.json`, global and project scope.
    Mcp,
    /// `mcp-auth.json`, global scope: OAuth state of MCP servers.
    McpAuth,
}

impl ConfigFile {
    /// File name inside the agent or project directory.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Settings => "settings.json",
            Self::Auth => "auth.json",
            Self::Models => "models.json",
            Self::Keybindings => "keybindings.json",
            Self::Mcp => "mcp.json",
            Self::McpAuth => "mcp-auth.json",
        }
    }

    /// Whether pi ends the file with a newline.
    pub fn trailing_newline(self) -> bool {
        matches!(self, Self::Keybindings | Self::Mcp | Self::McpAuth)
    }

    /// Serializes `document` as pi writes this file: two-space indent, then the
    /// trailing newline policy. pi re-detects the indent of an existing `mcp.json`;
    /// two spaces is its default.
    pub fn render<T: Serialize + ?Sized>(self, document: &T) -> serde_json::Result<String> {
        let mut text = json::to_string_pretty(document, "  ")?;
        if self.trailing_newline() {
            text.push('\n');
        }
        Ok(text)
    }
}
