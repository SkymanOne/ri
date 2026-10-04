//! Wire API implementations, one per API id, and dispatch by model.

pub mod anthropic;
pub mod bedrock;
pub mod classify;
pub mod google;
pub mod images;
pub mod mistral;
pub mod openai_completions;
pub mod openai_responses;
pub mod pi_messages;

use std::sync::Arc;

use indexmap::IndexMap;
use ri_types::message::{Content, ContentBlock, Message};

use crate::stream::{EventStream, Provider, Request, new_output, now_ms, send_error};

/// The built-in implementation of a wire API id.
pub fn builtin(api: &str) -> Option<Arc<dyn Provider>> {
    match api {
        "anthropic-messages" => Some(Arc::new(anthropic::AnthropicMessages)),
        "openai-completions" => Some(Arc::new(openai_completions::OpenAiCompletions)),
        "openai-responses" => Some(Arc::new(openai_responses::OpenAiResponses)),
        "azure-openai-responses" => Some(Arc::new(openai_responses::AzureOpenAiResponses)),
        "openai-codex-responses" => Some(Arc::new(openai_responses::OpenAiCodexResponses)),
        "google-generative-ai" => Some(Arc::new(google::GoogleGenerativeAi)),
        "google-vertex" => Some(Arc::new(google::GoogleVertex)),
        "mistral-conversations" => Some(Arc::new(mistral::MistralConversations)),
        "bedrock-converse-stream" => Some(Arc::new(bedrock::BedrockConverseStream)),
        "pi-messages" => Some(Arc::new(pi_messages::PiMessages)),
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

/// A stream that ends at once with `message` as its error.
pub fn failed_stream(
    model: &ri_types::model::Model,
    cancel: &tokio_util::sync::CancellationToken,
    message: String,
) -> EventStream {
    let (sender, stream) = EventStream::channel();
    send_error(&sender, new_output(model, now_ms()), cancel, message);
    stream
}

/// GitHub Copilot's per-request headers: whether the user or the agent
/// initiated the request, and the vision flag when images are sent. Port of
/// `github-copilot-headers.ts`.
pub(crate) fn copilot_headers(messages: &[Message]) -> Vec<(&'static str, String)> {
    let initiator = match messages.last() {
        Some(Message::User(_)) | None => "user",
        Some(_) => "agent",
    };
    let has_image = |blocks: &[ContentBlock]| {
        blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
    };
    let vision = messages.iter().any(|message| match message {
        Message::User(user) => {
            matches!(&user.content, Content::Blocks(blocks) if has_image(blocks))
        }
        Message::ToolResult(result) => has_image(&result.content),
        _ => false,
    });
    let mut headers = vec![
        ("X-Initiator", initiator.to_owned()),
        ("Openai-Intent", "conversation-edits".to_owned()),
    ];
    if vision {
        headers.push(("Copilot-Vision-Request", "true".to_owned()));
    }
    headers
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
        if matches!(
            request.model.provider.as_str(),
            "cloudflare-ai-gateway" | "cloudflare-workers-ai"
        ) {
            request.model.base_url = crate::key_auth::cloudflare_base_url(
                &request.model.base_url,
                request.options.env.as_ref(),
            );
        }
        match self.get(&request.model.api) {
            Some(provider) => provider.stream(request),
            None => {
                let message = format!("No API provider registered for api: {}", request.model.api);
                failed_stream(&request.model, &request.options.cancel, message)
            }
        }
    }
}
