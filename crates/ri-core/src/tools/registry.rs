//! The session's tools and which of them are active, as pi's `AgentSession`
//! keeps them (`_refreshToolRegistry`, `_applyToolLoadout` in pi `v1.0.0`).
//!
//! Active tools are declared to the model. Registering a `direct` or
//! `model-only` tool activates it unless it opts out; re-registering a name
//! replaces the tool in place, and a tool that becomes hidden is deactivated.
//! Names restored from the transcript before their tool is registered wait as
//! pending and activate on registration.
//!
//! `--tools`, `--no-tools` and `--exclude-tools` restrict which tools exist
//! at all, as pi's `_isAllowedTool`: tools outside them are never registered
//! or activated, whoever registers them.

use indexmap::IndexMap;

use super::{Exposure, RegisteredTool};

/// Every registered tool, in registration order, and the active set.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: IndexMap<String, RegisteredTool>,
    active: Vec<String>,
    pending: Vec<String>,
    /// The only tool names allowed, when restricted.
    allowed: Option<Vec<String>>,
    /// Tool names never allowed.
    excluded: Vec<String>,
}

impl ToolRegistry {
    /// `tools` registered, with `active` the active names among them.
    pub fn new(tools: Vec<RegisteredTool>, active: Vec<String>) -> ToolRegistry {
        let mut registry = ToolRegistry {
            tools: tools
                .into_iter()
                .map(|tool| (tool.name().to_owned(), tool))
                .collect(),
            active: Vec::new(),
            pending: Vec::new(),
            allowed: None,
            excluded: Vec::new(),
        };
        registry.apply(active);
        registry
    }

    /// Allows only `allowed` (every name when `None`) minus `excluded`;
    /// registered tools outside them are removed.
    pub fn restrict(&mut self, allowed: Option<Vec<String>>, excluded: Vec<String>) {
        self.allowed = allowed;
        self.excluded = excluded;
        let removed: Vec<String> = self
            .tools
            .keys()
            .filter(|name| !self.is_allowed(name))
            .cloned()
            .collect();
        for name in removed {
            self.tools.shift_remove(&name);
        }
        self.apply(self.active.clone());
    }

    /// pi's `_isAllowedTool`.
    pub fn is_allowed(&self, name: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|allowed| allowed == name))
            && !self.excluded.iter().any(|excluded| excluded == name)
    }

    fn activated_on_registration(tool: &RegisteredTool) -> bool {
        tool.exposure.declarable() && tool.default_active
    }

    /// Keeps the names of registered, non-hidden tools, once each.
    fn apply(&mut self, names: Vec<String>) {
        let mut active = Vec::new();
        for name in names {
            let usable = self
                .tools
                .get(&name)
                .is_some_and(|tool| tool.exposure != Exposure::Hidden);
            if usable && !active.contains(&name) {
                active.push(name);
            }
        }
        self.pending.retain(|name| !active.contains(name));
        self.active = active;
    }

    /// Registers or replaces a tool, then updates the active set.
    pub fn register(&mut self, tool: RegisteredTool) {
        let name = tool.name().to_owned();
        if !self.is_allowed(&name) {
            return;
        }
        let was_activated = self
            .tools
            .get(&name)
            .is_some_and(Self::activated_on_registration);
        // Naming a tool in `--tools` activates it even when it is not active
        // by default, as in pi.
        let named = self.allowed.is_some() && tool.exposure.declarable();
        let activate = !was_activated && (named || Self::activated_on_registration(&tool));
        self.tools.insert(name.clone(), tool);
        let mut next = self.active.clone();
        if activate {
            next.push(name);
        }
        next.extend(
            self.pending
                .iter()
                .filter(|pending| self.tools.contains_key(*pending))
                .cloned(),
        );
        self.apply(next);
    }

    /// Activates exactly `names`, ignoring unknown and hidden ones. Pending
    /// names are dropped when a previously active tool is deactivated.
    pub fn set_active(&mut self, names: Vec<String>) {
        let previous = self.active.clone();
        self.apply(names);
        if previous.iter().any(|name| !self.active.contains(name)) {
            self.pending.clear();
        }
    }

    /// Activates `names` as a transcript recorded them; names of tools not
    /// registered yet activate when they are.
    pub fn restore(&mut self, names: Vec<String>) {
        self.pending = names
            .iter()
            .filter(|name| !self.tools.contains_key(*name))
            .cloned()
            .collect();
        self.apply(names);
    }

    /// Names of the active tools, in activation order.
    pub fn active(&self) -> Vec<String> {
        self.active.clone()
    }

    /// The active tools, as declared to the model.
    pub fn declared(&self) -> Vec<RegisteredTool> {
        self.active
            .iter()
            .filter_map(|name| self.tools.get(name).cloned())
            .collect()
    }

    /// Every registered tool, in registration order.
    pub fn all(&self) -> Vec<RegisteredTool> {
        self.tools.values().cloned().collect()
    }

    /// The tool named `name`.
    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.get(name)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_util::future::BoxFuture;
    use ri_agent::{Tool, UpdateSink};
    use ri_types::event::ToolResult;
    use ri_types::message::ToolDeclaration;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct Named(ToolDeclaration);

    impl Tool for Named {
        fn declaration(&self) -> &ToolDeclaration {
            &self.0
        }

        fn execute(
            &self,
            _call_id: String,
            _args: Value,
            _cancel: CancellationToken,
            _updates: UpdateSink,
        ) -> BoxFuture<'_, Result<ToolResult, String>> {
            Box::pin(async { Err("unused".into()) })
        }
    }

    fn tool(name: &str, exposure: Exposure) -> RegisteredTool {
        RegisteredTool {
            exposure,
            ..RegisteredTool::direct(
                Arc::new(Named(ToolDeclaration {
                    name: name.into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                    constrained_sampling: None,
                })),
                None,
                Vec::new(),
            )
        }
    }

    #[test]
    fn activates_like_pi() {
        let mut registry = ToolRegistry::new(
            vec![
                tool("read", Exposure::Direct),
                tool("bash", Exposure::Direct),
            ],
            vec!["read".into(), "nope".into()],
        );
        assert_eq!(registry.active(), ["read"]);
        registry.register(tool("mcp__a", Exposure::Direct));
        registry.register(tool("mcp__b", Exposure::Deferred));
        assert_eq!(registry.active(), ["read", "mcp__a"]);
        // Re-registering a direct tool does not reactivate it.
        registry.set_active(vec!["read".into()]);
        registry.register(tool("mcp__a", Exposure::Direct));
        assert_eq!(registry.active(), ["read"]);
        // Hidden tools leave the active set; becoming direct again activates them.
        registry.set_active(vec!["read".into(), "mcp__a".into(), "mcp__b".into()]);
        registry.register(tool("mcp__a", Exposure::Hidden));
        assert_eq!(registry.active(), ["read", "mcp__b"]);
        registry.register(tool("mcp__a", Exposure::Direct));
        assert_eq!(registry.active(), ["read", "mcp__b", "mcp__a"]);
    }

    #[test]
    fn tool_flags_restrict_every_registration() {
        let mut registry = ToolRegistry::new(
            vec![
                tool("read", Exposure::Direct),
                tool("bash", Exposure::Direct),
            ],
            vec!["read".into(), "bash".into()],
        );
        // `--tools read,mcp__named`: nothing else exists, and a named tool
        // activates on registration.
        registry.restrict(Some(vec!["read".into(), "mcp__named".into()]), Vec::new());
        assert_eq!(registry.active(), ["read"]);
        registry.register(tool("codemode", Exposure::Direct));
        registry.register(tool("mcp__other", Exposure::Direct));
        registry.register(tool("mcp__named", Exposure::Deferred));
        assert!(registry.get("codemode").is_none());
        assert!(registry.get("mcp__other").is_none());
        registry.set_active(vec!["read".into(), "codemode".into()]);
        assert_eq!(registry.active(), ["read"]);
        // `--no-tools` allows none; `--exclude-tools` removes names.
        registry.restrict(Some(Vec::new()), Vec::new());
        registry.register(tool("mcp__a", Exposure::Direct));
        assert!(registry.active().is_empty());
        registry.restrict(None, vec!["bash".into()]);
        registry.register(tool("bash", Exposure::Direct));
        registry.register(tool("mcp__a", Exposure::Direct));
        assert_eq!(registry.active(), ["mcp__a"]);
    }

    #[test]
    fn restored_names_wait_for_registration() {
        let mut registry = ToolRegistry::new(vec![tool("read", Exposure::Direct)], Vec::new());
        registry.restore(vec!["read".into(), "mcp__late".into()]);
        assert_eq!(registry.active(), ["read"]);
        registry.register(tool("mcp__late", Exposure::Deferred));
        assert_eq!(registry.active(), ["read", "mcp__late"]);
    }
}
