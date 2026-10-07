//! Model selection, thinking levels and the model registry.

use std::sync::Arc;

use yapi_ai::registry::ModelRegistry;
use yapi_types::event::AgentEvent;
use yapi_types::extension_event::ExtensionEvent;
use yapi_types::message::ThinkingLevel;
use yapi_types::model::Model;
use yapi_types::sync::{lock, read, write};

use super::AgentSession;

/// Outcome of a model catalog refresh.
#[derive(Clone, Debug, Default)]
pub struct CatalogRefresh {
    /// Providers whose refresh failed, with why.
    pub errors: Vec<(String, String)>,
    /// Whether the refresh was cancelled before it finished.
    pub aborted: bool,
}

impl AgentSession {
    /// pi's `setModel`: switches to `model` and records it, then applies the
    /// thinking level for it. Fails when its provider has no credential.
    pub fn set_model(&self, model: Model) -> Result<(), String> {
        if !self.registry().has_auth(&model.provider) {
            return Err(format!("No API key for {}/{}", model.provider, model.id));
        }
        self.apply_model(model, None, "set");
        Ok(())
    }

    /// Takes over the model, thinking level and scope of the session this one
    /// replaces on `/reload`, which pi keeps without recording anything.
    pub fn keep_selection(&self, from: &AgentSession) {
        let (model, level) = {
            let state = lock(&from.inner.state);
            (state.model.clone(), state.thinking_level)
        };
        {
            let mut state = lock(&self.inner.state);
            state.model = model;
            state.thinking_level = level;
        }
        self.set_scoped_models(from.scoped_models());
    }

    /// The models cycling moves through, from `--models` or the
    /// `enabledModels` setting; empty when every available model is in scope.
    pub fn scoped_models(&self) -> Vec<crate::model_resolver::ScopedModel> {
        lock(&self.inner.scoped_models).clone()
    }

    /// The scoped models, or every available model when the scope is empty.
    pub fn models_in_scope(&self) -> Vec<Model> {
        let scoped = self.scoped_models();
        if scoped.is_empty() {
            self.available_models()
        } else {
            scoped.into_iter().map(|entry| entry.model).collect()
        }
    }

    /// Replaces the scope; an empty list puts every available model in it.
    pub fn set_scoped_models(&self, models: Vec<crate::model_resolver::ScopedModel>) {
        *lock(&self.inner.scoped_models) = models;
    }

    /// Saves `model` as the default in the global settings. A non-empty scope
    /// gains it, and so does a non-empty `enabledModels` setting, as in pi.
    pub fn save_default_model(&self, model: &Model) {
        let _ = self.set_global_setting(
            "defaultProvider",
            Some(serde_json::Value::String(model.provider.clone())),
        );
        let _ = self.set_global_setting(
            "defaultModel",
            Some(serde_json::Value::String(model.id.clone())),
        );
        {
            let mut scoped = lock(&self.inner.scoped_models);
            if scoped.is_empty()
                || scoped
                    .iter()
                    .any(|entry| entry.model.is(&model.provider, &model.id))
            {
                return;
            }
            scoped.push(crate::model_resolver::ScopedModel {
                model: model.clone(),
                thinking_level: None,
            });
        }
        let Some(enabled) = self
            .settings()
            .enabled_models
            .filter(|list| !list.is_empty())
        else {
            return;
        };
        let reference = model.reference();
        if enabled
            .iter()
            .any(|pattern| pattern.eq_ignore_ascii_case(&reference))
        {
            return;
        }
        let mut list: Vec<serde_json::Value> =
            enabled.into_iter().map(serde_json::Value::String).collect();
        list.push(serde_json::Value::String(reference));
        let _ = self.set_global_setting("enabledModels", Some(serde_json::Value::Array(list)));
    }

    /// Records `model` and applies the thinking level for it: `explicit`,
    /// else its level in settings, else the default level, else the current
    /// one. Extensions hear of a new model as pi's `model_select` from
    /// `source`.
    fn apply_model(&self, model: Model, explicit: Option<ThinkingLevel>, source: &str) {
        let settings = self.settings();
        let level = explicit
            .or_else(|| {
                settings
                    .model_thinking_levels
                    .as_ref()
                    .and_then(|levels| levels.get(&model.reference()))
                    .copied()
            })
            .or_else(|| lock(&self.inner.settings).default_thinking_level())
            .unwrap_or_else(|| self.thinking_level());
        let previous = {
            let mut state = lock(&self.inner.state);
            let _ = state
                .session
                .append_model_change(&model.provider, &model.id);
            state.model.replace(model.clone())
        };
        self.set_thinking_level(level);
        if previous
            .as_ref()
            .is_none_or(|previous| !previous.is(&model.provider, &model.id))
        {
            self.announce(ExtensionEvent::ModelSelect {
                model: &model,
                previous_model: previous.as_ref(),
                source,
            });
        }
    }

    /// pi's `cycleModel`, forward or backward, over the scoped models that
    /// have credentials, or over every available model when the scope is
    /// empty. Returns the new model and whether the scope applied; `None`
    /// when there is at most one model to cycle through.
    pub fn cycle_model(&self, forward: bool) -> Option<(Model, bool)> {
        let scoped = self.scoped_models();
        let available = self.available_models();
        let is_scoped = !scoped.is_empty();
        let models: Vec<(Model, Option<ThinkingLevel>)> = if is_scoped {
            scoped
                .into_iter()
                .filter(|entry| {
                    available
                        .iter()
                        .any(|m| m.is(&entry.model.provider, &entry.model.id))
                })
                .map(|entry| (entry.model, entry.thinking_level))
                .collect()
        } else {
            available.into_iter().map(|model| (model, None)).collect()
        };
        if models.len() <= 1 {
            return None;
        }
        let index = self
            .model()
            .and_then(|current| {
                models
                    .iter()
                    .position(|(model, _)| model.is(&current.provider, &current.id))
            })
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % models.len()
        } else {
            (index + models.len() - 1) % models.len()
        };
        let (model, level) = models[next].clone();
        self.apply_model(model.clone(), level, "cycle");
        Some((model, is_scoped))
    }

    /// Sets the thinking level, clamped to what the model supports; a change
    /// is recorded and announced.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        let mut state = lock(&self.inner.state);
        let level = match &state.model {
            Some(model) => yapi_ai::thinking::clamp_level(model, level),
            None => level,
        };
        if level == state.thinking_level {
            return;
        }
        let previous = std::mem::replace(&mut state.thinking_level, level);
        let _ = state.session.append_thinking_level_change(level.as_str());
        drop(state);
        self.emit(&AgentEvent::ThinkingLevelChanged { level });
        self.announce(ExtensionEvent::ThinkingLevelSelect {
            level,
            previous_level: previous,
        });
    }

    /// The thinking levels the current model supports; all of them without a
    /// model.
    pub fn available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        match self.model() {
            Some(model) => yapi_ai::thinking::supported_levels(&model),
            None => ThinkingLevel::ALL.to_vec(),
        }
    }

    /// pi's `cycleThinkingLevel`: the next supported level, wrapping. `None`
    /// when the model does not reason.
    pub fn cycle_thinking_level(&self) -> Option<ThinkingLevel> {
        let model = self.model()?;
        if !model.reasoning {
            return None;
        }
        let levels = yapi_ai::thinking::supported_levels(&model);
        let current = self.thinking_level();
        let index = levels.iter().position(|level| *level == current);
        let next = levels[index.map_or(0, |index| (index + 1) % levels.len())];
        self.set_thinking_level(next);
        Some(self.thinking_level())
    }

    /// Models with credentials, in catalog order.
    pub fn available_models(&self) -> Vec<Model> {
        read(&self.inner.registry)
            .available()
            .into_iter()
            .cloned()
            .collect()
    }

    /// The model registry; credentials it reads stay current with `auth.json`.
    pub fn registry(&self) -> Arc<ModelRegistry> {
        Arc::clone(&read(&self.inner.registry))
    }

    /// Refreshes the model catalogs that change between releases: restores
    /// the stored ones and, when `options` allow the network, fetches those of
    /// configured providers, as pi's `ModelRuntime.refresh`. The session then
    /// lists the new models. Concurrent refreshes run one after another.
    pub async fn refresh_model_catalogs(
        &self,
        options: yapi_ai::model_catalog::RefreshOptions,
    ) -> CatalogRefresh {
        let _running = self.inner.catalog_refresh.lock().await;
        let registry = self.registry();
        let Some(store) = registry.models_store().cloned() else {
            return CatalogRefresh::default();
        };
        let targets = registry.catalog_targets().await;
        let refreshed = yapi_ai::model_catalog::refresh(&targets, &store, &options).await;
        let mut lists = Vec::new();
        for extension in &self.inner.extensions {
            lists.extend(extension.refresh_models(&registry, &options).await);
        }
        let mut errors = refreshed.errors;
        // Apply to the registry current now; credentials may have changed.
        let mut slot = write(&self.inner.registry);
        let mut next = (**slot).clone();
        next.apply_catalogs(refreshed.models);
        for (provider, list) in lists {
            if let Err(error) = list.and_then(|models| next.replace_models(&provider, models))
                && !options.cancel.is_cancelled()
            {
                errors.push((provider, error));
            }
        }
        *slot = Arc::new(next);
        drop(slot);
        CatalogRefresh {
            errors,
            aborted: refreshed.aborted || options.cancel.is_cancelled(),
        }
    }

    /// Runs `f` with the model registry.
    pub fn with_registry<T>(&self, f: impl FnOnce(&ModelRegistry) -> T) -> T {
        f(&read(&self.inner.registry))
    }
}
