//! pi's built-in `llama.cpp` extension: the `/llama` command, which lists a
//! llama.cpp router's models, loads and unloads them, and downloads new ones
//! from Hugging Face. The provider itself lives in `yapi_ai::llama`.
//!
//! Port of `extensions/llama/index.ts` in pi-coding-agent `v1.0.0`. pi draws
//! the manager as its own components; yapi asks through the standard dialogs.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use yapi_ai::llama::{self, Client, ModelInfo, Progress, huggingface};
use yapi_ai::model_catalog::RefreshOptions;
use yapi_types::rpc::SourceInfo;

use crate::extensions::{
    Command, Context, DialogOptions, Extension, ExtensionUi, Mode, NotifyKind, builtin_source,
};

/// The extension's name, as `builtin:llama.cpp` selects it.
pub const NAME: &str = "llama.cpp";
const STATUS: &str = "llama.cpp";
const DOWNLOAD: &str = "Download a model…";

/// pi's built-in `llama.cpp` extension.
#[derive(Debug, Default)]
pub struct LlamaExtension;

fn context_label(model: &ModelInfo) -> Option<String> {
    let meta = model.meta.clone().unwrap_or_default();
    let label = |value: u64| {
        if value >= 1000 {
            format!("{}k", yapi_types::js::round(value as f64 / 1000.0))
        } else {
            value.to_string()
        }
    };
    if let Some(context) = meta.n_ctx.or(meta.n_ctx_train).filter(|value| *value > 0) {
        return Some(label(context));
    }
    let args = model.status.args.as_deref().unwrap_or_default();
    args.windows(2).find_map(|pair| {
        matches!(pair[0].as_str(), "--ctx-size" | "-c" | "-ctx")
            .then(|| pair[1].parse::<f64>().ok())
            .flatten()
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| label(value as u64))
    })
}

/// pi's `modelDescription`: the state, and the context of a loaded model.
fn description(model: &ModelInfo) -> String {
    let mut details = Vec::new();
    let loaded = model.is_loaded();
    if loaded {
        details.push("loaded".to_owned());
    } else if model.status.value != "unloaded" {
        details.push(model.status.value.clone());
    }
    if loaded && let Some(context) = context_label(model) {
        details.push(format!("{context} context"));
    }
    details.join(" · ")
}

fn row(model: &ModelInfo) -> String {
    let detail = description(model);
    if detail.is_empty() {
        model.id.clone()
    } else {
        format!("{}  {detail}", model.id)
    }
}

/// pi's `compactCount`.
fn compact_count(value: u64) -> String {
    let value = value as f64;
    if value >= 1_000_000.0 {
        let digits = usize::from(value < 10_000_000.0);
        format!("{}M", yapi_types::js::to_fixed(value / 1_000_000.0, digits))
    } else if value >= 1_000.0 {
        let digits = usize::from(value < 100_000.0);
        format!("{}k", yapi_types::js::to_fixed(value / 1_000.0, digits))
    } else {
        value.to_string()
    }
}

/// A connection failure reads as pi's "Could not connect to the server.".
fn connection_message(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("fetch failed") || lower.contains("timeout") || lower.contains("network") {
        "Could not connect to the server.".into()
    } else {
        error.to_owned()
    }
}

fn is_connection_error(error: &str) -> bool {
    connection_message(error) != error
}

fn progress_text(title: &str, model: &str, progress: &Progress) -> String {
    let mut text = format!("{title} {model}: {}", progress.message);
    if let Some(ratio) = progress.ratio {
        text.push_str(&format!(" {}%", yapi_types::js::round(ratio * 100.0)));
    }
    if let Some(detail) = &progress.detail {
        text.push_str(&format!(" ({detail})"));
    }
    text
}

struct Manager {
    ctx_ui: Arc<dyn ExtensionUi>,
    session: crate::agent_session::AgentSession,
    client: Client,
    cancel: CancellationToken,
}

impl Manager {
    /// The catalog, after the session's llama.cpp models follow it.
    async fn sync(&self) -> Result<Vec<ModelInfo>, String> {
        let catalog = self.client.list(false, &self.cancel).await?;
        self.refresh().await?;
        Ok(catalog)
    }

    async fn refresh(&self) -> Result<(), String> {
        let cancel = CancellationToken::new();
        let timer = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                cancel.cancel();
            })
        };
        // /llama already reached the server, so it refreshes even offline.
        let result = self
            .session
            .refresh_model_catalogs(RefreshOptions {
                providers: Some(vec![llama::PROVIDER_ID.into()]),
                allow_network: true,
                cancel,
                ..RefreshOptions::default()
            })
            .await;
        timer.abort();
        if result.aborted {
            return Err("Model catalog refresh timed out.".into());
        }
        match result.errors.into_iter().next() {
            Some((_, error)) => Err(error),
            None => Ok(()),
        }
    }

    /// The catalog, offering a retry while the server is unreachable.
    async fn read_catalog(&self) -> Option<Vec<ModelInfo>> {
        loop {
            match self.sync().await {
                Ok(catalog) => return Some(catalog),
                Err(error) => {
                    let title = format!(
                        "llama.cpp server unavailable\n{}\n\n{}",
                        self.client.server_url,
                        connection_message(&error)
                    );
                    let choice = self
                        .ctx_ui
                        .select(
                            &title,
                            vec!["Retry".into(), "Close".into()],
                            DialogOptions::default(),
                        )
                        .await;
                    if choice.as_deref() != Some("Retry") {
                        return None;
                    }
                }
            }
        }
    }

    fn status(&self, text: Option<&str>) {
        self.ctx_ui.set_status(STATUS, text);
    }

    async fn load(&self, catalog: &[ModelInfo], target: &ModelInfo) -> Result<(), String> {
        let loaded: Vec<&ModelInfo> = catalog
            .iter()
            .filter(|model| model.id != target.id && model.is_loaded())
            .collect();
        let mut replace = false;
        if !loaded.is_empty() {
            let title = format!(
                "{} model{} loaded",
                loaded.len(),
                if loaded.len() == 1 { " is" } else { "s are" }
            );
            let choice = self
                .ctx_ui
                .select(
                    &title,
                    vec![
                        "Unload all and load".into(),
                        "Keep loaded and load".into(),
                        "Cancel".into(),
                    ],
                    DialogOptions::default(),
                )
                .await;
            match choice.as_deref() {
                Some("Unload all and load") => replace = true,
                Some("Keep loaded and load") => {}
                _ => return Ok(()),
            }
        }
        if replace {
            for model in &loaded {
                self.client.unload_and_wait(&model.id, &self.cancel).await?;
            }
        }
        let ui = Arc::clone(&self.ctx_ui);
        let id = target.id.clone();
        let report = move |progress: Progress| {
            ui.set_status(STATUS, Some(&progress_text("Loading", &id, &progress)));
        };
        let result = self
            .client
            .load_and_wait(&target.id, &report, &self.cancel)
            .await;
        self.status(None);
        if let Err(error) = result {
            if replace {
                self.ctx_ui
                    .notify("Restoring previously loaded models", NotifyKind::Info);
                for model in &loaded {
                    let _ = self
                        .client
                        .load_and_wait(&model.id, &|_| {}, &self.cancel)
                        .await;
                }
                let _ = self.sync().await;
            }
            return Err(error);
        }
        let refreshed = self.sync().await?;
        let now_loaded = refreshed
            .iter()
            .any(|model| model.id == target.id && model.status.value == "loaded");
        self.ctx_ui.notify(
            &if now_loaded {
                format!("Loaded {}", target.id)
            } else {
                format!("Load started for {}", target.id)
            },
            NotifyKind::Info,
        );
        Ok(())
    }

    async fn unload(&self, model: &ModelInfo) -> Result<(), String> {
        if !self
            .ctx_ui
            .confirm("Unload model?", &model.id, DialogOptions::default())
            .await
        {
            return Ok(());
        }
        self.client.unload_and_wait(&model.id, &self.cancel).await?;
        self.sync().await?;
        self.ctx_ui
            .notify(&format!("Unloaded {}", model.id), NotifyKind::Info);
        Ok(())
    }

    async fn download(&self, home: Option<std::path::PathBuf>) -> Result<(), String> {
        let token = huggingface::find_token(&|name| std::env::var(name).ok(), home);
        let hub = huggingface::Client::new(token, None);
        let Some(query) = self
            .ctx_ui
            .input(
                "Download a model from Hugging Face",
                Some("Model name or owner/repository[:quant]"),
                DialogOptions::default(),
            )
            .await
            .map(|query| query.trim().to_owned())
            .filter(|query| !query.is_empty())
        else {
            return Ok(());
        };
        // `owner/repository[:quant]` names a model; anything else searches.
        let selected = if query.contains('/') {
            query
        } else {
            let results = hub.search(&query, &self.cancel).await?;
            if results.is_empty() {
                self.ctx_ui.notify(
                    &format!("No GGUF models match \"{query}\""),
                    NotifyKind::Warning,
                );
                return Ok(());
            }
            let labels: Vec<String> = results
                .iter()
                .map(|model| format!("{}  {} downloads", model.id, compact_count(model.downloads)))
                .collect();
            let Some(choice) = self
                .ctx_ui
                .select(
                    "Hugging Face models",
                    labels.clone(),
                    DialogOptions::default(),
                )
                .await
            else {
                return Ok(());
            };
            let Some(index) = labels.iter().position(|label| *label == choice) else {
                return Ok(());
            };
            results[index].id.clone()
        };
        let (repository, mut quantization) = match selected.find('/').and_then(|slash| {
            selected[slash + 1..]
                .find(':')
                .map(|colon| slash + 1 + colon)
        }) {
            Some(colon) => (
                selected[..colon].to_owned(),
                Some(selected[colon + 1..].to_owned()),
            ),
            None => (selected.clone(), None),
        };
        self.status(Some(&format!("Loading model details: {repository}")));
        let details = hub.details(&repository, &self.cancel).await;
        self.status(None);
        let details = details?;
        if let Some(gated) = &details.gated {
            let approval = if gated == "manual" {
                "Manual approval is required"
            } else {
                "Accept the access terms"
            };
            let title = format!(
                "Hugging Face access required\n{}\n\n{approval} at:\nhttps://huggingface.co/{}\n\nThe llama.cpp server needs HF_TOKEN with access.",
                details.id, details.id
            );
            let choice = self
                .ctx_ui
                .select(
                    &title,
                    vec!["Continue".into(), "Back".into()],
                    DialogOptions::default(),
                )
                .await;
            if choice.as_deref() != Some("Continue") {
                return Ok(());
            }
        }
        if quantization.is_none() && !details.quantizations.is_empty() {
            let options: Vec<String> = details
                .quantizations
                .iter()
                .map(|entry| {
                    let detail: Vec<String> = [
                        entry.size.map(|size| llama::format_bytes(size as f64)),
                        (entry.name == "Q4_K_M").then(|| "recommended".to_owned()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    if detail.is_empty() {
                        entry.name.clone()
                    } else {
                        format!("{} · {}", entry.name, detail.join(" · "))
                    }
                })
                .collect();
            let Some(choice) = self
                .ctx_ui
                .select(
                    &format!("Select quantization\n{}", details.id),
                    options.clone(),
                    DialogOptions::default(),
                )
                .await
            else {
                return Ok(());
            };
            let Some(index) = options.iter().position(|option| *option == choice) else {
                return Ok(());
            };
            quantization = Some(details.quantizations[index].name.clone());
        }
        let model = match quantization {
            Some(quantization) => format!("{}:{quantization}", details.id),
            None => details.id.clone(),
        };
        let ui = Arc::clone(&self.ctx_ui);
        let id = model.clone();
        let report = move |progress: Progress| {
            ui.set_status(STATUS, Some(&progress_text("Downloading", &id, &progress)));
        };
        let result = self
            .client
            .download_and_wait(&model, &report, &self.cancel)
            .await;
        self.status(None);
        result?;
        self.refresh().await?;
        self.ctx_ui
            .notify(&format!("Downloaded {model}"), NotifyKind::Info);
        Ok(())
    }
}

impl LlamaExtension {
    async fn manage(&self, ctx: &Context) {
        if ctx.mode != Mode::Tui {
            ctx.ui.notify(
                "/llama is available in interactive mode",
                NotifyKind::Warning,
            );
            return;
        }
        let Some(session) = ctx.session.upgrade() else {
            return;
        };
        let Some((server_url, api_key)) = session.registry().llama_server().await else {
            ctx.ui.notify(
                &format!("Configure llama.cpp with /login {}", llama::PROVIDER_ID),
                NotifyKind::Warning,
            );
            return;
        };
        let client = match Client::new(&server_url, api_key) {
            Ok(client) => client,
            Err(error) => {
                ctx.ui.notify(&error, NotifyKind::Error);
                return;
            }
        };
        let manager = Manager {
            ctx_ui: Arc::clone(&ctx.ui),
            session,
            client,
            cancel: CancellationToken::new(),
        };
        let Some(mut catalog) = manager.read_catalog().await else {
            return;
        };
        loop {
            let mut options: Vec<String> = catalog.iter().map(row).collect();
            options.push(DOWNLOAD.into());
            let title = format!("llama.cpp models\n{}", manager.client.server_url);
            let Some(choice) = ctx
                .ui
                .select(&title, options.clone(), DialogOptions::default())
                .await
            else {
                return;
            };
            let action = if choice == DOWNLOAD {
                manager.download(home_dir()).await
            } else {
                match options
                    .iter()
                    .position(|option| *option == choice)
                    .and_then(|index| catalog.get(index))
                {
                    Some(model) if model.is_loaded() => manager.unload(model).await,
                    Some(model) if model.status.value == "unloaded" => {
                        manager.load(&catalog, model).await
                    }
                    Some(model) => {
                        ctx.ui.notify(
                            &format!("{} is {}", model.id, model.status.value),
                            NotifyKind::Warning,
                        );
                        Ok(())
                    }
                    None => Ok(()),
                }
            };
            let Some(refreshed) = manager.read_catalog().await else {
                return;
            };
            catalog = refreshed;
            if let Err(error) = action
                && !is_connection_error(&error)
            {
                ctx.ui.notify(&error, NotifyKind::Error);
            }
        }
    }
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

impl Extension for LlamaExtension {
    fn source(&self) -> SourceInfo {
        builtin_source(NAME)
    }

    fn commands(&self) -> Vec<Command> {
        vec![Command {
            name: "llama".into(),
            description: "Manage llama.cpp router models".into(),
        }]
    }

    fn run_command<'a>(
        &'a self,
        command: &'a str,
        _args: &'a str,
        ctx: &'a Context,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if command == "llama" {
                self.manage(ctx).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_models_like_pi() {
        let model: ModelInfo = serde_json::from_value(serde_json::json!({
            "id": "qwen", "status": {"value": "loaded"}, "meta": {"n_ctx": 32768}
        }))
        .unwrap();
        assert_eq!(row(&model), "qwen  loaded · 33k context");
        let downloading: ModelInfo = serde_json::from_value(serde_json::json!({
            "id": "big", "status": {"value": "downloading"}
        }))
        .unwrap();
        assert_eq!(row(&downloading), "big  downloading");
        assert_eq!(compact_count(1_234_567), "1.2M");
        assert_eq!(compact_count(250_000), "250k");
        assert_eq!(compact_count(999), "999");
        assert_eq!(
            connection_message("fetch failed"),
            "Could not connect to the server."
        );
    }
}
