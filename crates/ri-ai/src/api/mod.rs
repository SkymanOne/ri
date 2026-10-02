//! Wire API implementations, one per API id, and dispatch by model.

pub mod anthropic;

use std::sync::Arc;

use indexmap::IndexMap;

use crate::stream::{EventStream, Provider, Request, new_output, now_ms, send_error};

/// The built-in implementation of a wire API id.
pub fn builtin(api: &str) -> Option<Arc<dyn Provider>> {
    match api {
        "anthropic-messages" => Some(Arc::new(anthropic::AnthropicMessages)),
        _ => None,
    }
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
