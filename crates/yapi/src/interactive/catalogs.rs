//! Model catalog refreshes in interactive mode: at startup, while a model
//! selector is open, for `/model <ref>` without a cached match, and after a
//! sign-in. pi's `refreshModelCatalogs` call sites in `interactive-mode.ts`
//! and `model-selector.ts`.

use yapi_ai::model_catalog::{REFRESH_TIMEOUT, RefreshOptions, cancel_after};
use yapi_core::agent_session::CatalogRefresh;

use super::login::ProviderOption;
use super::selectors::Selector;
use super::{App, Event};

/// Why a refresh ran, which decides what its result updates.
pub(super) enum Refresh {
    /// The background refresh after the first paint.
    Startup,
    /// The open model or scoped-models selector with this id.
    Selector(u64),
    /// `/model <ref>` found no cached match.
    ModelSearch(String),
    /// A sign-in finished; `defer` when the default model waits on the
    /// provider's catalog.
    Login {
        option: Box<ProviderOption>,
        label: String,
        defer: bool,
    },
}

/// What a selector shows about its refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RefreshStatus {
    Running,
    Done,
    /// Why the selector shows cached models.
    Failed(String),
}

impl RefreshStatus {
    pub(super) const RUNNING: &'static str = "Refreshing model catalogs…";
    pub(super) const DONE: &'static str = "Model catalogs refreshed.";
}

/// pi's messages for a refresh that timed out or failed, ending in `rest`.
fn failure(result: &CatalogRefresh, rest: &str) -> Option<String> {
    if result.aborted {
        return Some(format!("Model refresh timed out; {rest}"));
    }
    let names: Vec<&str> = result
        .errors
        .iter()
        .map(|(provider, _)| provider.as_str())
        .collect();
    (!names.is_empty()).then(|| format!("Could not refresh {}; {rest}", names.join(", ")))
}

impl App {
    /// Refreshes `providers` (all when `None`) in the background, cancelled
    /// after pi's timeout, and reports back as `purpose`.
    pub(super) fn refresh_catalogs(&mut self, providers: Option<Vec<String>>, purpose: Refresh) {
        let session = self.session.clone();
        let tx = self.tx.clone();
        let options = RefreshOptions {
            providers,
            allow_network: self.model_network,
            cancel: cancel_after(REFRESH_TIMEOUT),
            ..RefreshOptions::default()
        };
        tokio::spawn(async move {
            let result = session.refresh_model_catalogs(options).await;
            let _ = tx.send(Event::Catalogs(Box::new(purpose), result));
        });
    }

    /// The next selector refresh id.
    pub(super) fn next_refresh_id(&mut self) -> u64 {
        self.next_refresh += 1;
        self.next_refresh
    }

    /// A refresh finished.
    pub(super) fn on_catalogs(&mut self, purpose: Refresh, result: CatalogRefresh) {
        self.footer_cache = None;
        match purpose {
            Refresh::Startup => {}
            Refresh::Selector(id) => self.selector_refreshed(id, &result),
            Refresh::ModelSearch(reference) => {
                if let Some(warning) = failure(&result, "searching cached models.") {
                    self.warning(warning);
                }
                self.select_model_now(&reference);
            }
            Refresh::Login {
                option,
                label,
                defer,
            } => {
                if result.aborted {
                    self.warning(format!(
                        "{label}, but its model catalog refresh timed out; using cached models."
                    ));
                } else if !result.errors.is_empty() {
                    self.warning(format!(
                        "{label}, but its model catalog could not be refreshed; using cached models."
                    ));
                }
                // Keep a model chosen while the refresh ran.
                if defer && self.session.model().is_none() {
                    self.finish_login(&option, &label);
                }
            }
        }
    }

    fn selector_refreshed(&mut self, id: u64, result: &CatalogRefresh) {
        let models = self.session.available_models();
        let registry_error = self
            .session
            .with_registry(|registry| registry.error().map(str::to_owned))
            .flatten();
        match &mut self.selector {
            Some(Selector::Model(selector)) if selector.refresh_id == id => {
                let status = match failure(result, "showing cached models.") {
                    Some(message) => RefreshStatus::Failed(match result.errors.len() {
                        n if n > 1 && !result.aborted => format!(
                            "Could not refresh {n} model catalogs ({}); showing cached models.",
                            result
                                .errors
                                .iter()
                                .map(|(provider, _)| provider.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        _ => message,
                    }),
                    None => match registry_error {
                        Some(error) => RefreshStatus::Failed(error),
                        None => RefreshStatus::Done,
                    },
                };
                selector.refreshed(models, status);
            }
            Some(Selector::ScopedModels(selector)) if selector.refresh_id == id => {
                let status = match failure(result, "showing cached models.") {
                    Some(message) => RefreshStatus::Failed(message),
                    None => RefreshStatus::Done,
                };
                let follow_settings =
                    !selector.touched() && self.session.scoped_models().is_empty();
                let enabled = follow_settings.then(|| {
                    super::scoped_models::configured_ids(
                        &self.session.settings().enabled_models.unwrap_or_default(),
                        &models,
                    )
                });
                selector.refreshed(models, enabled, status);
                let enabled = selector.enabled().clone();
                if enabled.is_some() {
                    self.scoped_models_changed(enabled, false);
                }
            }
            _ => {}
        }
    }
}
