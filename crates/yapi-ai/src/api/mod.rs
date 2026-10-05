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
use yapi_types::message::{Content, ContentBlock, Message};

use crate::stream::{EventStream, Provider, Request, new_output, now_ms, send_error};

/// The built-in wire APIs. Each streams from a task that runs its module's
/// `run`.
#[derive(Clone, Copy, Debug)]
enum Builtin {
    Anthropic,
    Completions,
    Responses,
    AzureResponses,
    CodexResponses,
    Gemini,
    Vertex,
    Mistral,
    Bedrock,
    PiMessages,
}

impl Builtin {
    const ALL: [Builtin; 10] = [
        Builtin::Anthropic,
        Builtin::Completions,
        Builtin::Responses,
        Builtin::AzureResponses,
        Builtin::CodexResponses,
        Builtin::Gemini,
        Builtin::Vertex,
        Builtin::Mistral,
        Builtin::Bedrock,
        Builtin::PiMessages,
    ];
}

impl Provider for Builtin {
    fn api(&self) -> &str {
        match self {
            Builtin::Anthropic => "anthropic-messages",
            Builtin::Completions => "openai-completions",
            Builtin::Responses => "openai-responses",
            Builtin::AzureResponses => "azure-openai-responses",
            Builtin::CodexResponses => "openai-codex-responses",
            Builtin::Gemini => "google-generative-ai",
            Builtin::Vertex => "google-vertex",
            Builtin::Mistral => "mistral-conversations",
            Builtin::Bedrock => "bedrock-converse-stream",
            Builtin::PiMessages => "pi-messages",
        }
    }

    fn stream(&self, request: Request) -> EventStream {
        use google::Flavor as Google;
        use openai_responses::Flavor as Responses;
        let (sender, stream) = EventStream::channel();
        match self {
            Builtin::Anthropic => tokio::spawn(anthropic::run(request, sender)),
            Builtin::Completions => tokio::spawn(openai_completions::run(request, sender)),
            Builtin::Responses => {
                tokio::spawn(openai_responses::run(request, sender, Responses::OpenAi))
            }
            Builtin::AzureResponses => {
                tokio::spawn(openai_responses::run(request, sender, Responses::Azure))
            }
            Builtin::CodexResponses => {
                tokio::spawn(openai_responses::run(request, sender, Responses::Codex))
            }
            Builtin::Gemini => tokio::spawn(google::run(request, sender, Google::Gemini)),
            Builtin::Vertex => tokio::spawn(google::run(request, sender, Google::Vertex)),
            Builtin::Mistral => tokio::spawn(mistral::run(request, sender)),
            Builtin::Bedrock => tokio::spawn(bedrock::run(request, sender)),
            Builtin::PiMessages => tokio::spawn(pi_messages::run(request, sender)),
        };
        stream
    }
}

/// The built-in implementation of a wire API id.
pub fn builtin(api: &str) -> Option<Arc<dyn Provider>> {
    let builtin = Builtin::ALL
        .into_iter()
        .find(|builtin| builtin.api() == api)?;
    Some(Arc::new(builtin))
}

/// pi's `normalizeToolCallId` for Anthropic, Bedrock and Google:
/// [`sanitize_id_part`], at most 64 characters.
pub(crate) fn normalize_tool_call_id(id: &str) -> String {
    sanitize_id_part(id).chars().take(64).collect()
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
    model: &yapi_types::model::Model,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_tool_call_ids_in_utf16_units() {
        // pi's regex replaces each half of a surrogate pair.
        assert_eq!(normalize_tool_call_id("call|a😀b"), "call_a__b");
        assert_eq!(normalize_tool_call_id(&"x".repeat(70)), "x".repeat(64));
    }
}
