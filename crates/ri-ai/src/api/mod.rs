//! Wire API implementations, one per API id.

pub mod anthropic;

use std::sync::Arc;

use crate::stream::Provider;

/// The built-in implementation of a wire API id.
pub fn builtin(api: &str) -> Option<Arc<dyn Provider>> {
    match api {
        "anthropic-messages" => Some(Arc::new(anthropic::AnthropicMessages)),
        _ => None,
    }
}
