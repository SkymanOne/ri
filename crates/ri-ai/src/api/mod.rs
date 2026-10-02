//! Wire API implementations, one per API id, and dispatch by model.

pub mod anthropic;
pub mod google;
pub mod openai_completions;
pub mod openai_responses;

use std::sync::Arc;

use indexmap::IndexMap;

use crate::stream::{EventStream, Provider, Request, new_output, now_ms, send_error};

/// The built-in implementation of a wire API id.
pub fn builtin(api: &str) -> Option<Arc<dyn Provider>> {
    match api {
        "anthropic-messages" => Some(Arc::new(anthropic::AnthropicMessages)),
        "openai-completions" => Some(Arc::new(openai_completions::OpenAiCompletions)),
        "openai-responses" => Some(Arc::new(openai_responses::OpenAiResponses)),
        "google-generative-ai" => Some(Arc::new(google::GoogleGenerativeAi)),
        _ => None,
    }
}

/// Replaces every UTF-16 unit outside `[a-zA-Z0-9_-]` with `_`, as pi's id
/// sanitizers do; a character outside the BMP becomes two underscores.
pub(crate) fn sanitize_id_part(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let keep = c.is_ascii_alphanumeric() || c == '_' || c == '-';
            let (c, count) = if keep { (c, 1) } else { ('_', c.len_utf16()) };
            std::iter::repeat_n(c, count)
        })
        .collect()
}

/// Wire APIs by id: the built-ins plus any registered by extensions.
#[derive(Clone, Default)]
pub struct Apis {
    registered: IndexMap<String, Arc<dyn Provider>>,
}

impl Apis {
    /// Registers or replaces an implementation.
    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.registered.insert(provider.api().to_owned(), provider);
    }

    /// The implementation for an API id.
    pub fn get(&self, api: &str) -> Option<Arc<dyn Provider>> {
        self.registered.get(api).cloned().or_else(|| builtin(api))
    }

    /// Streams a request with the implementation of its model's API, adding the
    /// headers some providers expect. An unknown API ends the stream with an error.
    pub fn stream(&self, mut request: Request) -> EventStream {
        if matches!(request.model.provider.as_str(), "opencode" | "opencode-go")
            && let Some(session_id) = request.options.session_id.clone()
            && !request
                .options
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("x-opencode-session"))
        {
            request
                .options
                .headers
                .insert("x-opencode-session".into(), Some(session_id));
        }
        match self.get(&request.model.api) {
            Some(provider) => provider.stream(request),
            None => {
                let (sender, stream) = EventStream::channel();
                let message = format!("No API provider registered for api: {}", request.model.api);
                send_error(
                    &sender,
                    new_output(&request.model, now_ms()),
                    &request.options.cancel,
                    message,
                );
                stream
            }
        }
    }
}
